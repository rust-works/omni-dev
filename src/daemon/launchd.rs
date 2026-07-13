//! macOS launchd integration for `omni-dev daemon start` / `stop` and socket
//! activation.
//!
//! `start` writes a per-user LaunchAgent plist and bootstraps it. The agent is
//! **socket-activated**: launchd owns the control socket (declared in the plist's
//! `Sockets` dict) and spawns `omni-dev daemon run` the first time a client
//! connects, handing it the listening file descriptor via `launchd_listener`.
//! There is no `RunAtLoad`/`KeepAlive` — on-demand activation *is* the model, so
//! the parking bug `RunAtLoad` suffered in on-demand-only GUI sessions (the
//! `launchctl kickstart` workaround from #1078) cannot occur, and a crashed
//! daemon is re-activated on the next connect for free. A clean `daemon stop`
//! boots the agent out, removing the demand socket so the daemon stays down. See
//! ADR-0039 and issues #1078 / #1081.

use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::UnixListener;

use super::paths;

/// Reverse-DNS LaunchAgent label, derived from the project repository
/// (`github.com/rust-works/omni-dev`).
pub const LAUNCHD_LABEL: &str = "com.github.rust-works.omni-dev.daemon";

/// Path to the per-user LaunchAgent plist
/// (`~/Library/LaunchAgents/<label>.plist`).
pub fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

/// Renders a socket-activated LaunchAgent plist that execs
/// `omni-dev daemon run --socket <socket>`.
///
/// The `Sockets` → `Listener` dict makes launchd create and own the control
/// socket and demand-spawn the daemon on first connect; the daemon adopts the
/// inherited fd via [`launchd_listener`]. The socket path appears twice — as
/// launchd's `SockPathName` and on the daemon's own `--socket` argument — kept in
/// lock-step so the daemon resolves the same path for its co-located bridge token
/// file even though it binds the inherited fd rather than `SockPathName` itself.
/// `SockPathMode` `384` is decimal `0o600`: launchd creates the socket inode
/// owner-private from birth, the privacy `bind_private` enforced via umask on the
/// self-bound fallback path.
///
/// `StandardOutPath`/`StandardErrorPath` both point at the `daemon.log`
/// co-located with the socket ([`paths::log_path_for_socket`]), giving the
/// launchd-spawned daemon the same durable stdout/stderr sink the non-launchd
/// detached spawn already writes to; without it launchd sends the daemon's
/// lifecycle log lines to `/dev/null` (#1316). launchd creates that file under
/// its own umask, so the daemon tightens it to `0600` on startup
/// ([`super::server::run_with_shutdown`]).
///
/// Paths are XML-escaped before interpolation: `&`, `<`, `>` are all legal in
/// filenames (a home or `--socket` dir named `A&B`), and unescaped they would
/// produce malformed plist XML that `launchctl bootstrap` rejects with an opaque
/// parse error. A non-UTF-8 path cannot be faithfully represented in the UTF-8
/// plist, so it is rejected here rather than silently corrupted by
/// `Path::display()`. See issue #991.
fn render_plist(exe: &Path, socket: &Path) -> Result<String> {
    // The log path is derived from the socket's parent, so it is UTF-8 whenever
    // the socket is; validate it explicitly all the same to keep the plist
    // strictly UTF-8. Computed before `socket` is shadowed to `&str` below.
    let log = paths::log_path_for_socket(socket);
    let exe = exe
        .to_str()
        .with_context(|| format!("executable path is not valid UTF-8: {}", exe.display()))?;
    let socket = socket
        .to_str()
        .with_context(|| format!("socket path is not valid UTF-8: {}", socket.display()))?;
    let log = log
        .to_str()
        .with_context(|| format!("log path is not valid UTF-8: {}", log.display()))?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>run</string>
        <string>--socket</string>
        <string>{socket}</string>
    </array>
    <key>Sockets</key>
    <dict>
        <key>Listener</key>
        <dict>
            <key>SockPathName</key>
            <string>{socket}</string>
            <key>SockPathMode</key>
            <integer>384</integer>
            <key>SockFamily</key>
            <string>Unix</string>
            <key>SockType</key>
            <string>stream</string>
        </dict>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        // `LAUNCHD_LABEL` is a compile-time ASCII reverse-DNS constant with no
        // XML metacharacters, so it needs no escaping.
        label = LAUNCHD_LABEL,
        exe = quick_xml::escape::escape(exe),
        socket = quick_xml::escape::escape(socket),
        log = quick_xml::escape::escape(log),
    ))
}

/// The current user's launchd GUI domain target (`gui/<uid>`).
fn gui_domain() -> String {
    format!("gui/{}", nix::unistd::getuid())
}

/// The fully-qualified launchd service target (`<domain>/<label>`) passed to
/// `launchctl print` / `bootout`.
fn service_target(domain: &str) -> String {
    format!("{domain}/{LAUNCHD_LABEL}")
}

/// Writes the plist and bootstraps the agent so launchd listens on the demand
/// socket and spawns the daemon on the first client connect (and at login).
pub fn install_and_load(socket: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("could not resolve the current executable")?;
    let plist = plist_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&plist, render_plist(&exe, socket)?)
        .with_context(|| format!("failed to write {}", plist.display()))?;

    // launchd creates the demand socket at `SockPathName` when it bootstraps the
    // job — *before* our process runs — and does not create missing parent
    // directories. Ensure the owner-only (`0700`) runtime directory exists now so
    // that socket creation succeeds. See #1081.
    if let Some(parent) = socket.parent() {
        paths::ensure_dir_0700(parent)?;
    }

    let domain = gui_domain();
    // Bootout any prior instance first so bootstrap does not fail as already loaded.
    bootout(&domain);
    // `launchctl bootout` is asynchronous: it returns before launchd finishes
    // tearing the job down. Bootstrapping into that window races the teardown
    // and fails with EIO ("Bootstrap failed: 5: Input/output error") — the
    // failure `daemon restart` hits, since it boots the old agent out and
    // immediately re-bootstraps. Wait for the job to actually disappear first.
    wait_until_unloaded(&domain);

    // Even once the job is gone, launchd can briefly return a transient EIO
    // while it settles, so retry a few times before giving up.
    let mut last_err = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let output = Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&plist)
            .output()
            .context("failed to run `launchctl bootstrap`")?;
        if output.status.success() {
            // launchd now owns the demand socket and spawns the daemon on the
            // first client connect (`start`'s readiness ping triggers it). No
            // `kickstart` is needed — there is no `RunAtLoad` to fail. See #1081.
            return Ok(());
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }
    bail!("launchctl bootstrap failed: {last_err}");
}

/// Polls `launchctl print <domain>/<label>` until launchd no longer knows the
/// job (a non-zero exit / "Could not find service"), or ~5s elapses.
///
/// This closes the window opened by the asynchronous `launchctl bootout`: a
/// `bootstrap` issued while the prior job is still tearing down fails with EIO.
fn wait_until_unloaded(domain: &str) {
    let target = service_target(domain);
    for _ in 0..50 {
        let still_loaded = Command::new("launchctl")
            .args(["print", &target])
            .output()
            .is_ok_and(|out| out.status.success());
        if !still_loaded {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Boots out the agent (SIGTERM-ing a running daemon) so it stops and no longer
/// auto-starts. Best-effort: a not-loaded agent is not an error.
pub fn unload() -> Result<()> {
    bootout(&gui_domain());
    Ok(())
}

/// Runs `launchctl bootout <domain>/<label>`, the teardown relied on by
/// `start`/`restart` (clearing a prior instance before bootstrap, inside
/// `install_and_load`) and `stop` (disabling auto-start, via `unload`).
///
/// A non-zero exit is ignored on purpose: "already not loaded" is the common,
/// benign case. A *spawn* failure (e.g. `launchctl` missing or the GUI domain
/// unreachable) means the teardown silently did nothing, so it is logged rather
/// than discarded — otherwise `stop`/`restart` would claim to have disabled
/// auto-start when they had not. See issue #996.
fn bootout(domain: &str) {
    let target = service_target(domain);
    if let Err(e) = Command::new("launchctl")
        .args(["bootout", &target])
        .output()
    {
        tracing::warn!("failed to run `launchctl bootout {target}`: {e}");
    }
}

/// Adopts the listening socket launchd created for us when the daemon was
/// **socket-activated** (the plist's `Sockets` → `<name>` entry, `name =
/// "Listener"`).
///
/// Returns `Ok(None)` when the process was *not* launched by launchd with that
/// activation socket — the manual / dev / CI case (`omni-dev daemon run` from a
/// shell), where the caller falls back to binding the socket itself
/// ([`single_instance::bind_or_reclaim`](super::single_instance::bind_or_reclaim)).
///
/// launchd hands us a `malloc`-ed array of inherited fds; we take ownership of
/// the first (the plist declares exactly one), set it non-blocking, adopt it as a
/// Tokio listener, close any extra descriptors, and free the array. The symbol
/// lives in libSystem, so no `#[link]` attribute is required.
#[allow(unsafe_code)]
pub(crate) fn launchd_listener(name: &str) -> Result<Option<UnixListener>> {
    use nix::libc::{self, c_char, c_int, size_t};

    extern "C" {
        fn launch_activate_socket(
            name: *const c_char,
            fds: *mut *mut RawFd,
            cnt: *mut size_t,
        ) -> c_int;
    }

    let c_name = std::ffi::CString::new(name).context("launchd socket name had an interior NUL")?;
    let mut fds: *mut RawFd = std::ptr::null_mut();
    let mut cnt: size_t = 0;

    // SAFETY: we pass a valid C string and two valid out-pointers.
    // `launch_activate_socket` either writes a freshly `malloc`-ed array of `cnt`
    // ints into `*fds` and returns 0, or returns a non-zero errno and allocates
    // nothing. We never read past `cnt` and free the array exactly once below.
    let rc = unsafe { launch_activate_socket(c_name.as_ptr(), &mut fds, &mut cnt) };
    if rc != 0 {
        // ENOENT/ESRCH ⇒ no activation socket under this name (not launchd-spawned,
        // or the name does not match the plist): fall back to a self-bound socket.
        // Other errno values are equally non-fatal here. Nothing was allocated.
        tracing::debug!("launch_activate_socket({name}) returned {rc}; not socket-activated");
        return Ok(None);
    }

    // Defensive: a success with no descriptors. Free any allocation and fall back.
    if fds.is_null() || cnt == 0 {
        if !fds.is_null() {
            // SAFETY: non-null `fds` is the array launchd allocated; free it once.
            unsafe { libc::free(fds.cast()) };
        }
        return Ok(None);
    }

    // SAFETY: on success `fds` points at `cnt >= 1` ints; read the first, then
    // free the array exactly once. The fd stays valid after the array is freed.
    let raw = unsafe { *fds };

    // The plist declares exactly one `Listener`, so `cnt` is always 1 in practice.
    // Should the kernel ever hand us more, we adopt only the first; close the rest
    // so they are not leaked for the process lifetime.
    if cnt > 1 {
        tracing::warn!(
            "launch_activate_socket({name}) returned {cnt} descriptors; adopting the first and \
             closing {} extra",
            cnt - 1
        );
        for i in 1..cnt {
            // SAFETY: `fds` points at `cnt` ints; `i` is in `1..cnt`, so `fds.add(i)`
            // is in bounds. Each extra fd is owned by us and closed exactly once.
            let extra = unsafe { *fds.add(i) };
            unsafe { libc::close(extra) };
        }
    }

    // SAFETY: non-null `fds` is the array launchd allocated; free it exactly once.
    // The fds read above stay valid after the array itself is freed.
    unsafe { libc::free(fds.cast()) };

    // SAFETY: `raw` is a listening Unix-domain socket fd handed off by launchd and
    // now owned solely by us; adopting it into a std listener transfers ownership
    // (closed on drop). It is converted to a Tokio listener after being set
    // non-blocking, as the runtime requires.
    let std_listener = unsafe { StdUnixListener::from_raw_fd(raw) };
    std_listener
        .set_nonblocking(true)
        .context("failed to set the launchd socket non-blocking")?;
    let listener = UnixListener::from_std(std_listener)
        .context("failed to adopt the launchd socket into the Tokio runtime")?;
    Ok(Some(listener))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Parses `xml` to the end, asserting it is well-formed (no quick-xml
    /// error). This is the property that actually matters: an unescaped `&` in a
    /// path makes `launchctl bootstrap` reject the plist with an opaque parse
    /// error (issue #991).
    fn assert_well_formed(xml: &str) {
        use quick_xml::events::Event;
        use quick_xml::reader::Reader;

        let mut reader = Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("plist XML is not well-formed: {e}\n---\n{xml}"),
            }
        }
    }

    #[test]
    fn escapes_xml_metacharacters_in_paths() {
        // `&`, `<`, `>` are all legal in filenames but must be escaped in XML.
        let plist = render_plist(
            Path::new("/Users/A&B/bin/omni-dev"),
            Path::new("/tmp/<sock>/daemon.sock"),
        )
        .expect("ASCII paths render");

        // The metacharacters are escaped...
        assert!(plist.contains("/Users/A&amp;B/bin/omni-dev"), "{plist}");
        assert!(plist.contains("/tmp/&lt;sock&gt;/daemon.sock"), "{plist}");
        // ...and no raw metacharacter survives inside the path strings.
        assert!(!plist.contains("A&B"), "{plist}");
        assert!(!plist.contains("<sock>"), "{plist}");

        assert_well_formed(&plist);
    }

    #[test]
    fn renders_plain_paths_verbatim() {
        let plist = render_plist(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/omni-dev/daemon.sock"),
        )
        .expect("ASCII paths render");

        assert!(
            plist.contains("<string>/usr/local/bin/omni-dev</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/tmp/omni-dev/daemon.sock</string>"),
            "{plist}"
        );
        assert!(
            plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")),
            "{plist}"
        );
        assert_well_formed(&plist);
    }

    #[test]
    fn renders_a_socket_activated_agent() {
        let plist = render_plist(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/omni-dev/daemon.sock"),
        )
        .expect("ASCII paths render");

        // launchd owns a demand-activating `Listener` socket at the socket path,
        // created `0o600` (decimal 384) as a Unix stream.
        assert!(plist.contains("<key>Sockets</key>"), "{plist}");
        assert!(plist.contains("<key>Listener</key>"), "{plist}");
        assert!(
            plist.contains(
                "<key>SockPathName</key>\n            <string>/tmp/omni-dev/daemon.sock</string>"
            ),
            "{plist}"
        );
        assert!(
            plist.contains("<key>SockPathMode</key>\n            <integer>384</integer>"),
            "{plist}"
        );
        assert!(plist.contains("<string>Unix</string>"), "{plist}");
        assert!(plist.contains("<string>stream</string>"), "{plist}");

        // The pre-#1081 run-at-load model is gone: there is nothing for launchd to
        // park, and a clean stop (bootout) is what keeps the daemon down.
        assert!(!plist.contains("RunAtLoad"), "{plist}");
        assert!(!plist.contains("KeepAlive"), "{plist}");

        assert_well_formed(&plist);
    }

    #[test]
    fn sinks_stdio_to_the_daemon_log_beside_the_socket() {
        // Without a std-stream sink launchd discards the daemon's stdout/stderr to
        // /dev/null, so its lifecycle log lines never reach an operator (#1316).
        // Both streams point at the `daemon.log` co-located with the socket — the
        // exact path the non-launchd detached-spawn launcher already writes.
        let plist = render_plist(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/omni-dev/daemon.sock"),
        )
        .expect("ASCII paths render");

        assert!(
            plist.contains(
                "<key>StandardOutPath</key>\n    <string>/tmp/omni-dev/daemon.log</string>"
            ),
            "{plist}"
        );
        assert!(
            plist.contains(
                "<key>StandardErrorPath</key>\n    <string>/tmp/omni-dev/daemon.log</string>"
            ),
            "{plist}"
        );
        assert_eq!(
            paths::log_path_for_socket(Path::new("/tmp/omni-dev/daemon.sock")),
            Path::new("/tmp/omni-dev/daemon.log"),
            "the sink must match the launcher's log path"
        );
        assert_well_formed(&plist);
    }

    /// Run outside launchd (the unit-test binary is not socket-activated), so the
    /// activation lookup must report "no inherited socket" rather than error,
    /// letting the daemon fall back to self-binding. This exercises the real FFI;
    /// it returns `Ok(None)` before touching the Tokio reactor, so no runtime is
    /// needed here.
    #[test]
    fn launchd_listener_is_none_when_not_activated() {
        let result = launchd_listener("Listener");
        assert!(
            matches!(result, Ok(None)),
            "expected Ok(None) outside launchd, got {result:?}"
        );
    }

    #[test]
    fn service_target_joins_domain_and_label() {
        // The launchctl service target is `<domain>/<label>` — the form passed to
        // `print` / `bootout`.
        assert_eq!(
            service_target("gui/501"),
            format!("gui/501/{LAUNCHD_LABEL}")
        );
    }

    #[test]
    fn rejects_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // 0xFF is not valid UTF-8; such a path cannot be placed in the UTF-8
        // plist, so it must be rejected rather than silently corrupted.
        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xFF/omni-dev"));
        assert!(render_plist(bad, Path::new("/tmp/daemon.sock")).is_err());
        assert!(render_plist(Path::new("/usr/bin/omni-dev"), bad).is_err());
    }
}
