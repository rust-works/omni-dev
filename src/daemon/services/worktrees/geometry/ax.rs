//! The macOS Accessibility backend: the only place in omni-dev with `unsafe`
//! beyond the daemon's socket-activation FFI.
//!
//! The crate sets `unsafe_code = "deny"`, and STYLE-0013 allows an exception only
//! when it is justified in an ADR ([ADR-0058]), isolated in a dedicated module
//! (this one), and carries `SAFETY:` comments — the same shape as the daemon's
//! `launchd_listener` and the `setsid` `pre_exec` hook. (Those are named without
//! doc links deliberately: both are macOS-only items, so linking them would break
//! a documentation build on any other platform.)
//!
//! # Why the Accessibility API
//!
//! Moving another application's window is not something a normal process may do;
//! macOS gates it behind the **Accessibility** permission (System Settings →
//! Privacy & Security → Accessibility). Two properties make AX the right
//! mechanism rather than merely an available one:
//!
//! 1. **Setting `AXPosition`/`AXSize` neither raises nor activates a window**, so
//!    Z-order is preserved for free. That is a hard requirement of #1407 and the
//!    opposite of the existing focus path, which spawns `code <folder>` and lets
//!    VS Code raise the window.
//! 2. AX can also **read** a window's geometry, which is how the reference frame
//!    is obtained — an extension cannot read its own bounds.
//!
//! # Scope of what this can do
//!
//! Deliberately minimal, and that minimality is the security argument (ADR-0058):
//! it reads window titles/geometry/flags and writes geometry, for processes owned
//! by the same user, and does nothing else. It never synthesises input, never
//! reads window contents, never raises or closes a window, and never touches a
//! process the caller did not name by pid.
//!
//! # Ownership and reference counting
//!
//! Every AX object is a CoreFoundation type, so `CFRetained<CFType>` from
//! `objc2-core-foundation` owns them: `CFRelease` runs on drop, and there is no
//! hand-written `Drop` to get wrong. Values read out of an attribute are
//! `downcast`-checked against their concrete CF type rather than blind-cast, so a
//! surprise from the platform is a `None`, not undefined behaviour.
//!
//! [ADR-0058]: https://github.com/rust-works/omni-dev/blob/main/docs/adrs/adr-0058.md

// Nothing is imported at this level, and both implementations below pull what
// they need from the parent module themselves. That is deliberate: every item here
// would otherwise be live on exactly one platform and dead — a `-D warnings`
// failure — on the other.

#[cfg(target_os = "macos")]
pub(crate) use macos::AxBackend;

#[cfg(not(target_os = "macos"))]
pub(crate) use unsupported::AxBackend;

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};

    use objc2_core_foundation::{
        CFArray, CFBoolean, CFEqual, CFRetained, CFString, CFType, CGPoint, CGSize, Type,
    };

    use super::super::{Frame, OsWindow, WindowBackend, WindowId};

    /// Resolves `pid -> owning application pid` with one batched `ps` call.
    ///
    /// The only process introspection the op needs, and deliberately a shell-out
    /// rather than more FFI: an extension-host pid's parent is the VS Code main
    /// process that owns every `NSWindow`, so one `ps` resolves a whole selection.
    /// A `ps` that fails outright yields an empty map, which every target then
    /// reports as not-found — never a silent mis-target.
    ///
    /// Lives here, alongside its only caller, rather than in the platform-independent
    /// parent: the whole `ppid` strategy is specific to how this backend locates a
    /// window, and on any other platform it would simply be dead code.
    fn app_pids_via_ps(pids: &[u32]) -> HashMap<u32, u32> {
        if pids.is_empty() {
            return HashMap::new();
        }
        let list = pids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        match std::process::Command::new("/bin/ps")
            .args(["-o", "pid=,ppid=", "-p", &list])
            .output()
        {
            Ok(out) => parse_ppids(&String::from_utf8_lossy(&out.stdout)),
            Err(err) => {
                tracing::warn!("cannot resolve owning applications via ps: {err}");
                HashMap::new()
            }
        }
    }

    /// Parses `ps -o pid=,ppid= -p …` output into a `pid -> ppid` map.
    ///
    /// Unparseable lines are skipped rather than failing the batch — a pid that has
    /// since exited simply gets no entry, which its target reports as not-found.
    fn parse_ppids(stdout: &str) -> HashMap<u32, u32> {
        stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let ppid = fields.next()?.parse().ok()?;
                Some((pid, ppid))
            })
            .collect()
    }

    /// How long to wait for an application to answer one AX request.
    ///
    /// Load-bearing rather than defensive: AX is synchronous Mach messaging into
    /// another process, so without an explicit timeout a wedged or beachballing VS
    /// Code would block this thread indefinitely — and this runs on a daemon
    /// blocking thread, not a throwaway one. Generous enough that a merely busy
    /// app still answers, short enough that a hung one degrades to a per-target
    /// `failed` instead of hanging the op.
    const MESSAGING_TIMEOUT_SECS: f32 = 2.0;

    /// `kAXErrorSuccess`.
    const AX_SUCCESS: i32 = 0;
    /// `kAXValueTypeCGPoint`.
    const AX_VALUE_CGPOINT: u32 = 1;
    /// `kAXValueTypeCGSize`.
    const AX_VALUE_CGSIZE: u32 = 2;

    /// The `AXSubrole` a normal document window reports.
    const SUBROLE_STANDARD_WINDOW: &str = "AXStandardWindow";

    // The Accessibility C API. Declared inline rather than pulled in as a
    // generated-bindings crate: seven entry points, all plain C over
    // `CFTypeRef`, is a smaller and far more reviewable surface than a whole
    // framework binding — the `launchd_listener` precedent.
    //
    // `Boolean` is C's `unsigned char`, so it is declared `u8` and compared
    // against zero rather than transmuted to `bool`.
    #[allow(unsafe_code)]
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXUIElementCreateApplication(pid: i32) -> *mut c_void;
        fn AXUIElementCopyAttributeValue(
            element: *mut c_void,
            attribute: *const CFString,
            value: *mut *const c_void,
        ) -> i32;
        fn AXUIElementSetAttributeValue(
            element: *mut c_void,
            attribute: *const CFString,
            value: *const c_void,
        ) -> i32;
        fn AXUIElementSetMessagingTimeout(element: *mut c_void, timeout: f32) -> i32;
        fn AXValueCreate(value_type: u32, value: *const c_void) -> *const c_void;
        fn AXValueGetValue(value: *const c_void, value_type: u32, out: *mut c_void) -> u8;
    }

    /// An owned AX object (an application element, a window element, or an
    /// `AXValue`).
    ///
    /// Held as `CFRetained<CFType>` because every AX type is a CoreFoundation type
    /// and `CFRetained` already implements exactly the right `CFRelease`-on-drop
    /// semantics. `AXUIElementRef` has no binding of its own in
    /// `objc2-core-foundation`, but it does not need one: nothing here calls a
    /// method *on* the element, only passes it back to the C API.
    type AxObject = CFRetained<CFType>;

    /// Wraps a pointer an AX `Copy`/`Create` call returned with +1 retain count.
    ///
    /// Returns `None` for null, which is how every AX failure path arrives here.
    #[allow(unsafe_code)]
    fn own(ptr: *const c_void) -> Option<AxObject> {
        let ptr = NonNull::new(ptr.cast_mut())?;
        // SAFETY: AX `…Copy…`/`…Create…` functions follow the CoreFoundation
        // create rule, so the caller owns one reference to a valid CF object.
        // `CFRetained::from_raw` takes exactly that ownership and releases it on
        // drop, so the +1 is balanced once and only once.
        Some(unsafe { CFRetained::from_raw(ptr.cast::<CFType>()) })
    }

    /// The raw pointer for an owned AX object, for handing back to the C API.
    /// Borrowing only — the returned pointer is valid for as long as `object` is,
    /// and never carries ownership.
    fn raw(object: &AxObject) -> *mut c_void {
        CFRetained::as_ptr(object).as_ptr().cast::<c_void>()
    }

    /// Reads one attribute of an AX element.
    ///
    /// A missing or unsupported attribute is `None`, not an error: `AXFullScreen`
    /// genuinely does not exist on every window, and a window that closed
    /// mid-enumeration answers `kAXErrorInvalidUIElement`. Both mean "no value",
    /// which the caller treats as a sensible default.
    #[allow(unsafe_code)]
    fn attribute(element: *mut c_void, name: &str) -> Option<AxObject> {
        let name = CFString::from_str(name);
        let mut value: *const c_void = ptr::null();
        // SAFETY: `element` is a live AX element (owned by the caller for the
        // duration of this call), `name` is a valid `CFStringRef` alive across the
        // call, and `value` is a writable slot for the single out-pointer the API
        // fills on success. On any non-success return the slot is left untouched,
        // so it is only read below when the status says it was written.
        let status = unsafe {
            AXUIElementCopyAttributeValue(element, CFRetained::as_ptr(&name).as_ptr(), &mut value)
        };
        if status == AX_SUCCESS {
            own(value)
        } else {
            None
        }
    }

    /// Reads a string attribute.
    fn string_attribute(element: *mut c_void, name: &str) -> Option<String> {
        let value = attribute(element, name)?;
        Some(value.downcast_ref::<CFString>()?.to_string())
    }

    /// Reads a boolean attribute, defaulting to `false` when it is absent — which
    /// is the correct reading for `AXMinimized`/`AXFullScreen` on a window whose
    /// application does not expose them.
    fn bool_attribute(element: *mut c_void, name: &str) -> bool {
        attribute(element, name)
            .and_then(|value| value.downcast_ref::<CFBoolean>().map(CFBoolean::value))
            .unwrap_or(false)
    }

    /// Reads an `AXValue`-wrapped geometry attribute into `T`.
    ///
    /// `T` must be the struct the given `value_type` describes — `CGPoint` for
    /// `AX_VALUE_CGPOINT`, `CGSize` for `AX_VALUE_CGSIZE` — which the two callers
    /// below pair correctly and nothing else calls.
    #[allow(unsafe_code)]
    fn value_attribute<T: Default>(element: *mut c_void, name: &str, value_type: u32) -> Option<T> {
        let value = attribute(element, name)?;
        let mut out = T::default();
        // SAFETY: `value` is a live `AXValueRef` owned for this call, and `out` is
        // a properly aligned, fully initialised `T` whose type matches
        // `value_type` at both call sites — so the API writes exactly
        // `size_of::<T>()` bytes into a slot that large. The return value is
        // checked, so `out` is only used when AX says it wrote the value.
        let ok = unsafe {
            AXValueGetValue(
                raw(&value).cast_const(),
                value_type,
                (&raw mut out).cast::<c_void>(),
            )
        };
        (ok != 0).then_some(out)
    }

    /// Writes an `AXValue`-wrapped geometry attribute.
    ///
    /// Same type/`value_type` pairing contract as [`value_attribute`].
    #[allow(unsafe_code)]
    fn set_value_attribute<T>(
        element: *mut c_void,
        name: &str,
        value_type: u32,
        value: &T,
    ) -> Result<(), i32> {
        // SAFETY: `value` is a live, aligned `T` matching `value_type`, so
        // `AXValueCreate` reads exactly `size_of::<T>()` bytes from it and copies
        // them into a new +1 `AXValueRef` — which `own` takes responsibility for
        // releasing. A null return (allocation failure) becomes an error below.
        let wrapped = unsafe { AXValueCreate(value_type, (&raw const *value).cast::<c_void>()) };
        let Some(wrapped) = own(wrapped) else {
            return Err(i32::MIN);
        };
        let name = CFString::from_str(name);
        // SAFETY: `element` is a live AX element, `name` a valid `CFStringRef`, and
        // `wrapped` a valid `AXValueRef`; all three outlive the call, and the API
        // borrows rather than consumes the value (`wrapped` is still released by
        // its own drop).
        let status = unsafe {
            AXUIElementSetAttributeValue(
                element,
                CFRetained::as_ptr(&name).as_ptr(),
                raw(&wrapped).cast_const(),
            )
        };
        if status == AX_SUCCESS {
            Ok(())
        } else {
            Err(status)
        }
    }

    /// The macOS window backend.
    ///
    /// Enumerating an application yields both the plain [`OsWindow`] snapshots the
    /// pure planner reasons over **and** the live element handles a later
    /// `set_frame` needs, so the two cannot drift out of step: a [`WindowId`]
    /// indexes into the very list its snapshot came from. The cache is
    /// per-instance and the instance lives only for one op, so it can never serve
    /// a stale window.
    pub(crate) struct AxBackend {
        /// Live window elements per application pid, parallel to the snapshots
        /// returned by [`WindowBackend::windows`].
        elements: RefCell<HashMap<u32, Vec<AxObject>>>,
    }

    impl AxBackend {
        /// Creates a backend for a single op. Cheap — no AX call until used.
        pub(crate) fn new() -> Self {
            Self {
                elements: RefCell::new(HashMap::new()),
            }
        }

        /// The application element for `app_pid`, with a messaging timeout set.
        #[allow(unsafe_code)]
        fn application(app_pid: u32) -> Option<AxObject> {
            // SAFETY: a plain constructor over an integer pid; it allocates a new
            // +1 element (or null for a pid that is gone), which `own` adopts.
            let app = own(unsafe { AXUIElementCreateApplication(app_pid as i32) })?;
            // SAFETY: `app` is the live element just created and outlives the call.
            unsafe { AXUIElementSetMessagingTimeout(raw(&app), MESSAGING_TIMEOUT_SECS) };
            Some(app)
        }

        /// Snapshots one window element.
        fn snapshot(window: &AxObject, focused: Option<&AxObject>) -> Option<OsWindow> {
            let element = raw(window);
            let position: CGPoint = value_attribute(element, "AXPosition", AX_VALUE_CGPOINT)?;
            let size: CGSize = value_attribute(element, "AXSize", AX_VALUE_CGSIZE)?;
            let subrole = string_attribute(element, "AXSubrole");
            Some(OsWindow {
                title: string_attribute(element, "AXTitle").unwrap_or_default(),
                frame: Frame {
                    x: position.x,
                    y: position.y,
                    width: size.width,
                    height: size.height,
                },
                minimized: bool_attribute(element, "AXMinimized"),
                fullscreen: bool_attribute(element, "AXFullScreen"),
                // A window that reports no subrole at all is treated as standard:
                // the flag exists to exclude things that positively identify as
                // sheets or palettes, not to require an opt-in.
                standard: subrole.map_or(true, |role| role == SUBROLE_STANDARD_WINDOW),
                // CFEqual rather than pointer equality: AX may hand out distinct
                // element objects that refer to the same window.
                focused: focused.is_some_and(|f| CFEqual(Some(window), Some(f))),
            })
        }
    }

    impl WindowBackend for AxBackend {
        fn trusted(&self) -> bool {
            // SAFETY: an argument-less predicate with no pointers involved. The
            // prompting variant is deliberately not used — a permission dialog
            // from a background agent is unreliable, so the caller reports the
            // untrusted state and lets the UI offer the settings deep-link.
            #[allow(unsafe_code)]
            let trusted = unsafe { AXIsProcessTrusted() };
            trusted != 0
        }

        fn app_pids(&self, pids: &[u32]) -> HashMap<u32, u32> {
            app_pids_via_ps(pids)
        }

        fn windows(&self, app_pid: u32) -> Result<Vec<OsWindow>, String> {
            let app = Self::application(app_pid)
                .ok_or_else(|| format!("no accessible application for process {app_pid}"))?;
            let element = raw(&app);
            let list = attribute(element, "AXWindows").ok_or_else(|| {
                format!(
                    "process {app_pid} exposed no windows (is Accessibility granted to omni-dev?)"
                )
            })?;
            let list = list
                .downcast_ref::<CFArray>()
                .ok_or_else(|| format!("process {app_pid} returned a non-array window list"))?;
            // The application's focused window, used only to break a reference tie.
            let focused = attribute(element, "AXFocusedWindow");

            let mut snapshots = Vec::new();
            let mut elements = Vec::new();
            for index in 0..list.count() {
                // SAFETY: `index` is within `0..count` of a live CFArray that
                // outlives the call, so this returns a borrowed (+0) element
                // pointer, which `Type::retain` promotes to an owned reference
                // rather than adopting a count it was not given.
                #[allow(unsafe_code)]
                let item = unsafe { list.value_at_index(index) };
                let Some(item) = NonNull::new(item.cast_mut()) else {
                    continue;
                };
                // SAFETY: the array holds `AXUIElementRef`s, i.e. CF objects, and
                // the pointer is valid for as long as the array is — which is
                // until the end of this function, strictly after the retain.
                #[allow(unsafe_code)]
                let window = unsafe { item.cast::<CFType>().as_ref() }.retain();
                if let Some(snapshot) = Self::snapshot(&window, focused.as_ref()) {
                    snapshots.push(snapshot);
                    elements.push(window);
                }
            }
            self.elements.borrow_mut().insert(app_pid, elements);
            Ok(snapshots)
        }

        fn set_frame(&self, id: WindowId, frame: Frame) -> Result<Frame, String> {
            let elements = self.elements.borrow();
            let window = elements
                .get(&id.app_pid)
                .and_then(|list| list.get(id.index))
                .ok_or_else(|| "the window was not enumerated by this operation".to_string())?;
            let element = raw(window);

            let size = CGSize {
                width: frame.width,
                height: frame.height,
            };
            let position = CGPoint {
                x: frame.x,
                y: frame.y,
            };
            // Size, then position, then size again. A window can refuse part of a
            // move while it is still too large for the display it is landing on,
            // and refuse part of a resize while it is still pinned near an edge;
            // one extra pass settles both. Only the *last* failure is reported,
            // since an earlier partial write can be corrected by a later one.
            let mut last_error = None;
            for step in [
                set_value_attribute(element, "AXSize", AX_VALUE_CGSIZE, &size),
                set_value_attribute(element, "AXPosition", AX_VALUE_CGPOINT, &position),
                set_value_attribute(element, "AXSize", AX_VALUE_CGSIZE, &size),
            ] {
                if let Err(status) = step {
                    last_error = Some(status);
                }
            }

            // Read back rather than assume: this is what turns a window that
            // clamped itself to a minimum size into an honest `partial` report.
            let read = || -> Option<Frame> {
                let position: CGPoint = value_attribute(element, "AXPosition", AX_VALUE_CGPOINT)?;
                let size: CGSize = value_attribute(element, "AXSize", AX_VALUE_CGSIZE)?;
                Some(Frame {
                    x: position.x,
                    y: position.y,
                    width: size.width,
                    height: size.height,
                })
            };
            match (read(), last_error) {
                (Some(actual), _) => Ok(actual),
                (None, Some(status)) => {
                    Err(format!("the window rejected the move (AXError {status})"))
                }
                (None, None) => Err("the window's geometry could not be read back".to_string()),
            }
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod tests {
        use super::*;

        #[test]
        fn parses_ps_output() {
            // The real shape of `ps -o pid=,ppid= -p …`: right-aligned, leading
            // spaces.
            let out = "  28112  65488\n  28113  65488\n  65488      1\n";
            let map = parse_ppids(out);
            assert_eq!(map.get(&28112).copied(), Some(65488));
            assert_eq!(map.get(&65488).copied(), Some(1));
            assert_eq!(map.len(), 3);
        }

        #[test]
        fn ignores_unparseable_ps_lines() {
            // A pid that has exited makes `ps` emit a diagnostic line rather than a
            // row; skipping it leaves that target reporting not-found instead of
            // sinking the whole batch.
            let out = "\n  ps: 999: no such process\n  10  20\nnotanumber x\n";
            assert_eq!(parse_ppids(out), HashMap::from([(10, 20)]));
        }

        #[test]
        fn resolving_no_pids_never_shells_out() {
            assert!(app_pids_via_ps(&[]).is_empty());
        }

        #[test]
        fn ps_resolves_this_process_to_its_real_parent() {
            // The one test that exercises the real `ps` seam, using the only pids
            // guaranteed to exist: this process and its parent.
            let me = std::process::id();
            let resolved = app_pids_via_ps(&[me]);
            assert_eq!(
                resolved.get(&me).copied(),
                Some(std::os::unix::process::parent_id()),
                "ps should report this test process's real parent"
            );
        }

        #[test]
        fn an_unknown_pid_simply_has_no_entry() {
            // pid 0 is never reported by `ps -p`, so this exercises the "resolution
            // failed for this pid" path that a target reports as not-found.
            assert!(!app_pids_via_ps(&[0]).contains_key(&0));
        }

        /// Smoke-tests the Accessibility FFI itself against the live framework.
        ///
        /// This is the one test that can catch a wrong `extern "C"` signature, a
        /// mis-declared `AXValueType`, a bad `downcast`, or an unbalanced retain — none
        /// of which the fake-backend tests can see, since they never cross the FFI
        /// boundary. It deliberately asserts nothing about the *result*: without the
        /// Accessibility grant (CI, and any developer who has not opted in) enumeration
        /// correctly fails, and with it the window list depends on what happens to be
        /// running. What is being asserted is that **calling it is sound** — it returns
        /// either way rather than crashing, hanging, or corrupting memory, which is
        /// exactly what a signature or ownership mistake would do.
        ///
        /// Runs against this test process, so there is always a live pid to target.
        #[test]
        fn the_accessibility_ffi_is_callable_without_a_grant() {
            use super::super::super::WindowBackend;

            let backend = AxBackend::new();
            // Reaches AXIsProcessTrusted; both answers are legitimate.
            let _ = backend.trusted();
            // Reaches AXUIElementCreateApplication, AXUIElementSetMessagingTimeout,
            // AXUIElementCopyAttributeValue, the CFRetained ownership dance, and the
            // CFArray/CFString downcasts. A test binary is not a GUI app, so `Ok` with
            // no windows and `Err` are both expected outcomes.
            match backend.windows(std::process::id()) {
                Ok(windows) => assert!(
                    windows.iter().all(|w| w.frame.width >= 0.0),
                    "a window reported a negative width, so the CGSize decode is wrong"
                ),
                Err(err) => assert!(!err.is_empty(), "a failure should say why"),
            }
            // And the write path's own guard: an id this backend never enumerated must
            // be refused rather than dereferenced.
            let unknown = WindowId {
                app_pid: std::process::id(),
                index: 9_999,
            };
            assert!(backend
                .set_frame(
                    unknown,
                    Frame {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                    },
                )
                .is_err());
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod unsupported {
    use std::collections::HashMap;

    use super::super::{Frame, OsWindow, WindowBackend, WindowId};

    /// The non-macOS backend: reports itself untrusted so every op short-circuits
    /// with the same "no permission to move windows" reply path a macOS daemon
    /// without the Accessibility grant produces.
    ///
    /// v1 is macOS-only (ADR-0058), consistent with the tray. Linux/X11 via EWMH
    /// title matching and Windows via `SetWindowPos` are the documented
    /// follow-ups; each would replace this with a real implementation of the same
    /// four methods, leaving every caller and the whole planner untouched.
    pub(crate) struct AxBackend;

    impl AxBackend {
        /// Creates the no-op backend.
        pub(crate) fn new() -> Self {
            Self
        }
    }

    impl WindowBackend for AxBackend {
        fn trusted(&self) -> bool {
            false
        }

        fn app_pids(&self, _pids: &[u32]) -> HashMap<u32, u32> {
            HashMap::new()
        }

        fn windows(&self, _app_pid: u32) -> Result<Vec<OsWindow>, String> {
            Err("window repositioning is only implemented on macOS".to_string())
        }

        fn set_frame(&self, _id: WindowId, _frame: Frame) -> Result<Frame, String> {
            Err("window repositioning is only implemented on macOS".to_string())
        }
    }
}
