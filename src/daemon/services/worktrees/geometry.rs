//! Window geometry: the platform-independent half of the `reposition` op (#1407).
//!
//! A VS Code extension cannot read or set its window's OS position or size —
//! there is no API and the request is parked upstream (microsoft/vscode#28150,
//! #83170, #195406). So repositioning has to happen in a native process, and the
//! daemon already *is* that process: a GUI-session agent that is the only vantage
//! point knowing every window (ADR-0040). See [ADR-0058].
//!
//! Everything decidable **without** touching the platform lives here: which OS
//! window belongs to which registered worktree window, what the reference
//! geometry is, and what outcome each target gets. The platform capability itself
//! is the narrow [`WindowBackend`] trait, whose only real implementation is the
//! `unsafe` Accessibility FFI isolated in [`ax`] (STYLE-0013). So the planning,
//! matching, and reporting below are plain-struct unit-testable on every
//! platform — including the ones where no backend exists at all.
//!
//! # Why title matching
//!
//! The registry stores no OS window id and no geometry, and its `pid` is the
//! **extension-host** process. In Electron every window's native `NSWindow`
//! belongs to the single **main** process, so `pid` cannot locate a window. What
//! it *can* do is name the app: the extension host is a direct child of the main
//! process, so `ppid(pid)` is the owning application — which also attributes each
//! window to its own variant (Stable / Insiders / VSCodium) rather than guessing.
//! Within that application, windows are matched by their **root name** (the
//! registry's `title`, i.e. `vscode.workspace.name`) against `AXTitle`, using the
//! progressively-looser cascade in [`match_window`] that **refuses to guess**: an
//! ambiguous target is reported, never moved. `ppid` resolution itself is one
//! batched `ps` call ([`app_pids_via_ps`]) rather than more FFI.
//!
//! [ADR-0058]: https://github.com/rust-works/omni-dev/blob/main/docs/adrs/adr-0058.md

pub(super) mod ax;

use std::collections::{HashMap, HashSet};

use serde::Serialize;

/// How far a window's actual frame may sit from the requested one and still count
/// as a match, in points.
///
/// Not a tolerance for sloppiness — it is what distinguishes "the window moved"
/// from "the window refused". VS Code enforces a minimum window size, and a
/// window can be denied part of a move by the display it lands on, so a readback
/// outside this margin is a real, reportable [`OUTCOME_PARTIAL`] rather than a
/// rounding artefact. One point is well below anything a user could notice and
/// well above the sub-point rounding the platform performs.
const MATCH_EPSILON: f64 = 1.0;

/// A window's on-screen rectangle in the Accessibility API's global coordinate
/// space: origin at the **top-left of the primary display**, `y` increasing
/// downward, in points (not pixels — a Retina display reports logical points).
///
/// Because reads and writes share this space, "same position" is well-defined
/// across multiple displays: copying a frame verbatim moves the target onto
/// whichever display holds that rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(super) struct Frame {
    /// Left edge, in global points.
    pub(super) x: f64,
    /// Top edge, in global points.
    pub(super) y: f64,
    /// Width, in points.
    pub(super) width: f64,
    /// Height, in points.
    pub(super) height: f64,
}

impl Frame {
    /// Whether `self` and `other` describe the same rectangle to within
    /// [`MATCH_EPSILON`] on every edge.
    fn matches(self, other: Self) -> bool {
        (self.x - other.x).abs() <= MATCH_EPSILON
            && (self.y - other.y).abs() <= MATCH_EPSILON
            && (self.width - other.width).abs() <= MATCH_EPSILON
            && (self.height - other.height).abs() <= MATCH_EPSILON
    }
}

/// One of an application's windows as the platform backend sees it.
///
/// Deliberately a plain data snapshot rather than a live handle: the planner is
/// pure, so it reasons over these and emits [`WindowId`]s that index back into
/// the same enumeration the backend still holds.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct OsWindow {
    /// The window's title, as displayed in its title bar (`AXTitle`).
    pub(super) title: String,
    /// Its current position and size.
    pub(super) frame: Frame,
    /// Whether it is minimized into the Dock (`AXMinimized`). Geometry writes on
    /// a minimized window are undefined, so it is skipped.
    pub(super) minimized: bool,
    /// Whether it is in native fullscreen (`AXFullScreen`). A resize there is a
    /// no-op, so it is skipped.
    pub(super) fullscreen: bool,
    /// Whether it is a standard document window (`AXSubrole` is
    /// `AXStandardWindow`) rather than a sheet, palette, or dialog. Only standard
    /// windows are match candidates, so a native open/save sheet cannot be
    /// mistaken for the window behind it.
    pub(super) standard: bool,
    /// Whether the owning application reports this as its focused window
    /// (`AXFocusedWindow`). Used **only** as a last-resort tie-break when
    /// resolving the reference, never for a target.
    pub(super) focused: bool,
}

/// A registered window distilled to what matching needs: its registry key, its
/// root name, and the extension-host pid whose parent owns the OS window.
///
/// Every field below `key` can be absent, and none of those absences is an error:
/// the key may name a window that has closed, a brand-new empty window reports no
/// title, and a companion may omit `pid`. Each gets its own reportable outcome
/// rather than being silently dropped or conflated with the others.
#[derive(Debug, Clone)]
pub(super) struct RegisteredWindow {
    /// The companion-owned per-window key.
    pub(super) key: String,
    /// Whether the registry holds a live window under this key at all.
    ///
    /// A tree row can be a tick stale and name a window that has since closed —
    /// much the commonest skip in practice, and a different thing from a window
    /// that is open but cannot be named. Kept explicit rather than inferred from
    /// the two fields below, so the two never get conflated in a report.
    pub(super) live: bool,
    /// The window's root name (`vscode.workspace.name`).
    pub(super) title: Option<String>,
    /// The reporting extension-host pid.
    pub(super) pid: Option<u32>,
}

/// A resolved OS window: which application enumeration it came from, and its
/// index within it. Only meaningful to the backend that produced the enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct WindowId {
    /// The owning **application** pid (already resolved through `ppid`).
    pub(super) app_pid: u32,
    /// Index into that application's enumerated window list.
    pub(super) index: usize,
}

/// A target resolved to exactly one movable OS window.
#[derive(Debug, Clone)]
pub(super) struct PlannedMove {
    /// Position in the caller's target list, so results can be reported in the
    /// order asked for regardless of how planning grouped them.
    pub(super) index: usize,
    /// The registry key of the window to move.
    pub(super) key: String,
    /// Its root name, for display.
    pub(super) title: String,
    /// The OS window to write to.
    pub(super) window: WindowId,
    /// Where it sits **now**, so the executor can skip a no-op write and the op
    /// can record an undo entry.
    pub(super) from: Frame,
}

/// The per-target outcome reported back to the caller.
///
/// Follows the shape of the `close` op's `Note`: a stable machine `outcome` slug
/// plus a human `detail` line, so a summary can be written from the reply without
/// the client knowing every slug.
#[derive(Debug, Clone, Serialize)]
pub(super) struct TargetResult {
    /// Position in the caller's target list.
    #[serde(skip)]
    pub(super) index: usize,
    /// The registry key this result is about.
    pub(super) key: String,
    /// The window's root name, when it reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) title: Option<String>,
    /// A stable machine slug — one of the `OUTCOME_*` constants.
    pub(super) outcome: &'static str,
    /// A human-readable one-line explanation.
    pub(super) detail: String,
    /// The window's frame after the op, when one was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) frame: Option<Frame>,
}

/// The window was moved to the wanted frame.
pub(super) const OUTCOME_MOVED: &str = "moved";
/// A dry run resolved this window and would have moved it.
pub(super) const OUTCOME_WOULD_MOVE: &str = "would-move";
/// The window already had the wanted frame; nothing was written.
pub(super) const OUTCOME_UNCHANGED: &str = "unchanged";
/// The write was accepted but the window settled somewhere else (a minimum size,
/// or a display that refused the position).
pub(super) const OUTCOME_PARTIAL: &str = "partial";
/// No live window is registered under this key — the row naming it was stale.
pub(super) const OUTCOME_NO_WINDOW: &str = "no-window";
/// The registration carries no pid, so its application cannot be located.
pub(super) const OUTCOME_NO_PID: &str = "no-pid";
/// The registration carries no root name, so there is nothing to match on.
pub(super) const OUTCOME_NO_TITLE: &str = "no-title";
/// No OS window of the owning application matched the root name.
pub(super) const OUTCOME_NOT_FOUND: &str = "not-found";
/// More than one OS window matched and no rule separated them; refused.
pub(super) const OUTCOME_AMBIGUOUS: &str = "ambiguous";
/// The window is minimized, where geometry writes are undefined.
pub(super) const OUTCOME_MINIMIZED: &str = "minimized";
/// The window is in native fullscreen, where a resize is a no-op.
pub(super) const OUTCOME_FULLSCREEN: &str = "fullscreen";
/// The target is the reference window itself, which never moves.
pub(super) const OUTCOME_REFERENCE: &str = "reference";
/// The platform rejected the write.
pub(super) const OUTCOME_FAILED: &str = "failed";

impl TargetResult {
    /// A result for a [`PlannedMove`] that resolved to a window.
    fn resolved(
        planned: &PlannedMove,
        outcome: &'static str,
        detail: impl Into<String>,
        frame: Option<Frame>,
    ) -> Self {
        Self {
            index: planned.index,
            key: planned.key.clone(),
            title: Some(planned.title.clone()),
            outcome,
            detail: detail.into(),
            frame,
        }
    }
}

/// The reference window the whole batch copies from.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ResolvedReference {
    /// Its registry key.
    pub(super) key: String,
    /// Its root name.
    pub(super) title: String,
    /// The geometry every target is moved to.
    pub(super) frame: Frame,
}

/// Why the whole batch could not proceed. Distinct from a per-target skip: with
/// no reference geometry there is nothing to copy, so no target is attempted.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Blocked {
    /// A stable machine slug — one of the `BLOCKED_*` constants.
    pub(super) reason: &'static str,
    /// A human-readable one-line explanation.
    pub(super) detail: String,
}

/// The reference registration carries no pid, no root name, or a pid whose
/// application could not be resolved.
pub(super) const BLOCKED_UNIDENTIFIABLE: &str = "reference-unidentifiable";
/// No OS window matched the reference's root name.
pub(super) const BLOCKED_NOT_FOUND: &str = "reference-not-found";
/// Several OS windows matched the reference and none could be separated.
pub(super) const BLOCKED_AMBIGUOUS: &str = "reference-ambiguous";
/// The reference is in native fullscreen, so its frame is a whole display and
/// copying it to other windows is meaningless.
pub(super) const BLOCKED_FULLSCREEN: &str = "reference-fullscreen";
/// The reference is minimized, so it has no meaningful on-screen frame.
pub(super) const BLOCKED_MINIMIZED: &str = "reference-minimized";

/// The platform capability the op needs, and the whole of it.
///
/// Narrow on purpose: everything above this line is pure, so the trait is the
/// single seam where `unsafe` FFI (or, in a test, a fake) plugs in. Failures are
/// `String` rather than `anyhow::Error` because each one is destined for a
/// per-target `detail` line, not for an error chain.
pub(super) trait WindowBackend {
    /// Whether this process holds the OS permission required to inspect and move
    /// other applications' windows. Checked once up front: without it every
    /// single call would fail identically, which is a batch-level fact, not a
    /// per-target one.
    fn trusted(&self) -> bool;

    /// Maps each given process id to the id of the application process that owns
    /// its windows. A pid that cannot be resolved is simply absent.
    fn app_pids(&self, pids: &[u32]) -> HashMap<u32, u32>;

    /// Enumerates one application's windows, in the platform's order.
    fn windows(&self, app_pid: u32) -> Result<Vec<OsWindow>, String>;

    /// Moves and resizes a window, returning the frame it actually settled on.
    fn set_frame(&self, id: WindowId, frame: Frame) -> Result<Frame, String>;
}

/// Everything the `reposition` / `reposition-undo` ops report back.
#[derive(Debug)]
pub(super) struct RepositionReport {
    /// Whether the daemon holds the OS permission. When false nothing was
    /// attempted and every other field is empty — the client turns this into an
    /// actionable "grant Accessibility" prompt rather than a generic failure.
    pub(super) trusted: bool,
    /// Set when the reference could not be resolved, so no target was attempted.
    pub(super) blocked: Option<Blocked>,
    /// The reference geometry, when it resolved.
    pub(super) reference: Option<ResolvedReference>,
    /// One entry per requested target, in request order.
    pub(super) results: Vec<TargetResult>,
    /// `(key, prior frame)` for every window this op actually moved, for the undo
    /// store. Empty for a dry run.
    pub(super) undo: Vec<(String, Frame)>,
}

impl RepositionReport {
    /// The report for a daemon that lacks the OS permission.
    fn untrusted() -> Self {
        Self {
            trusted: false,
            blocked: None,
            reference: None,
            results: Vec::new(),
            undo: Vec::new(),
        }
    }

    /// How many targets were actually moved (fully or partially).
    pub(super) fn moved(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == OUTCOME_MOVED || r.outcome == OUTCOME_PARTIAL)
            .count()
    }

    /// How many targets were left alone, for whatever reason.
    pub(super) fn skipped(&self) -> usize {
        self.results.len() - self.moved()
    }
}

/// Repositions every target onto the reference window's frame.
///
/// **Blocking** — the backend does platform I/O, so callers run this on a blocking
/// thread. With `check` set nothing is written: every target that resolves is
/// reported [`OUTCOME_WOULD_MOVE`] instead. That dry run is the whole diagnostic
/// surface for title matching (`worktrees reposition --dry-run`), which is why it
/// walks exactly the same resolution path as a real run.
pub(super) fn reposition(
    backend: &dyn WindowBackend,
    reference: &RegisteredWindow,
    targets: &[RegisteredWindow],
    check: bool,
) -> RepositionReport {
    if !backend.trusted() {
        return RepositionReport::untrusted();
    }
    let (windows, app_pids) = enumerate(backend, std::iter::once(reference).chain(targets));

    let (reference_window, reference_info) = match resolve_reference(reference, &windows, &app_pids)
    {
        Ok(resolved) => resolved,
        Err(blocked) => {
            return RepositionReport {
                trusted: true,
                blocked: Some(blocked),
                reference: None,
                results: Vec::new(),
                undo: Vec::new(),
            }
        }
    };

    let (moves, mut results) = plan_moves(targets, &windows, &app_pids, Some(reference_window));
    let wanted = reference_info.frame;
    let mut undo = Vec::new();
    for planned in &moves {
        results.push(apply(backend, planned, wanted, check, &mut undo));
    }
    RepositionReport {
        trusted: true,
        blocked: None,
        reference: Some(reference_info),
        results: in_request_order(results),
        undo,
    }
}

/// Restores each window to the frame recorded for it by an earlier
/// [`reposition`].
///
/// Deliberately re-resolves every window from scratch rather than trusting a
/// stale handle: between the move and the undo a window may have closed, been
/// renamed, or gone fullscreen, and each of those is a reportable per-target
/// outcome. There is no reference here — each entry carries its own target frame.
///
/// **Blocking**, like [`reposition`].
pub(super) fn restore(
    backend: &dyn WindowBackend,
    entries: &[(RegisteredWindow, Frame)],
) -> RepositionReport {
    if !backend.trusted() {
        return RepositionReport::untrusted();
    }
    let targets: Vec<RegisteredWindow> = entries.iter().map(|(w, _)| w.clone()).collect();
    let (windows, app_pids) = enumerate(backend, targets.iter());
    let (moves, mut results) = plan_moves(&targets, &windows, &app_pids, None);
    let mut undo = Vec::new();
    for planned in &moves {
        let wanted = entries[planned.index].1;
        results.push(apply(backend, planned, wanted, false, &mut undo));
    }
    RepositionReport {
        trusted: true,
        blocked: None,
        reference: None,
        results: in_request_order(results),
        undo,
    }
}

/// Resolves the owning application of every given window and enumerates each
/// application's windows exactly once, however many registrations share it.
fn enumerate<'a>(
    backend: &dyn WindowBackend,
    registered: impl Iterator<Item = &'a RegisteredWindow>,
) -> (HashMap<u32, Vec<OsWindow>>, HashMap<u32, u32>) {
    // Deduplicated, because several windows of the same application share a pid
    // and every resolution below is per-application, not per-window.
    let pids: HashSet<u32> = registered.filter_map(|w| w.pid).collect();
    let pids: Vec<u32> = pids.into_iter().collect();
    let app_pids = backend.app_pids(&pids);

    let mut windows = HashMap::new();
    let apps: HashSet<u32> = app_pids.values().copied().collect();
    for app_pid in apps {
        match backend.windows(app_pid) {
            Ok(list) => {
                windows.insert(app_pid, list);
            }
            // Not silent: an application we cannot enumerate leaves its targets
            // reporting `not-found`, and the reason why is in the daemon log.
            Err(err) => tracing::warn!(app_pid, "cannot enumerate windows: {err}"),
        }
    }
    (windows, app_pids)
}

/// Writes one planned move (or, in a dry run, reports what it would have done),
/// appending an undo entry when a window really was moved.
fn apply(
    backend: &dyn WindowBackend,
    planned: &PlannedMove,
    wanted: Frame,
    check: bool,
    undo: &mut Vec<(String, Frame)>,
) -> TargetResult {
    // Decided before the write, not after a redundant one, so an idempotent
    // re-run touches no windows at all.
    if planned.from.matches(wanted) {
        return TargetResult::resolved(
            planned,
            OUTCOME_UNCHANGED,
            "already in the wanted position",
            Some(planned.from),
        );
    }
    if check {
        return TargetResult::resolved(
            planned,
            OUTCOME_WOULD_MOVE,
            "resolved to one window; would be moved",
            Some(planned.from),
        );
    }
    let result = move_result(planned, wanted, backend.set_frame(planned.window, wanted));
    // A partial move changed the window too, so it must be undoable.
    if result.outcome == OUTCOME_MOVED || result.outcome == OUTCOME_PARTIAL {
        undo.push((planned.key.clone(), planned.from));
    }
    result
}

/// Resolves the reference key to one OS window plus the frame to copy.
fn resolve_reference(
    reference: &RegisteredWindow,
    windows: &HashMap<u32, Vec<OsWindow>>,
    app_pids: &HashMap<u32, u32>,
) -> Result<(WindowId, ResolvedReference), Blocked> {
    let title = reference.title.as_deref().filter(|t| !t.trim().is_empty());
    let (true, Some(title), Some(pid)) = (reference.live, title, reference.pid) else {
        return Err(Blocked {
            reason: BLOCKED_UNIDENTIFIABLE,
            detail: "the invoking window is not registered, or reported no name or process id"
                .to_string(),
        });
    };
    let Some(app_pid) = app_pids.get(&pid).copied() else {
        return Err(Blocked {
            reason: BLOCKED_UNIDENTIFIABLE,
            detail: format!("cannot resolve the application owning process {pid}"),
        });
    };
    let candidates = windows.get(&app_pid).map_or(&[][..], Vec::as_slice);

    // The reference is the one window allowed the focus tie-break: whichever
    // window the user invoked from is its application's focused window, so focus
    // separates same-named windows that no title rule can.
    let index = match match_window(candidates, title, true) {
        Match::One(index) => index,
        Match::None => {
            return Err(Blocked {
                reason: BLOCKED_NOT_FOUND,
                detail: format!("no window of the invoking application is named “{title}”"),
            })
        }
        Match::Many(count) => {
            return Err(Blocked {
                reason: BLOCKED_AMBIGUOUS,
                detail: format!("{count} windows match the invoking window's name “{title}”"),
            })
        }
    };
    let window = &candidates[index];
    if window.fullscreen {
        return Err(Blocked {
            reason: BLOCKED_FULLSCREEN,
            detail: "the invoking window is in fullscreen, so its frame is a whole display"
                .to_string(),
        });
    }
    if window.minimized {
        return Err(Blocked {
            reason: BLOCKED_MINIMIZED,
            detail: "the invoking window is minimized, so it has no on-screen frame".to_string(),
        });
    }
    Ok((
        WindowId { app_pid, index },
        ResolvedReference {
            key: reference.key.clone(),
            title: title.to_string(),
            frame: window.frame,
        },
    ))
}

/// Resolves every target to at most one OS window, partitioning them into the
/// movable and the reportably-skipped.
///
/// Each resolved window is **claimed**, so no two targets can be planned onto the
/// same OS window and — when `reference_window` is given — none can be planned
/// onto the reference. Without a reference (the undo path) nothing is pre-claimed.
fn plan_moves(
    targets: &[RegisteredWindow],
    windows: &HashMap<u32, Vec<OsWindow>>,
    app_pids: &HashMap<u32, u32>,
    reference_window: Option<WindowId>,
) -> (Vec<PlannedMove>, Vec<TargetResult>) {
    let mut claimed: HashSet<WindowId> = reference_window.into_iter().collect();
    let mut moves = Vec::new();
    let mut skipped = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        match plan_target(
            index,
            target,
            windows,
            app_pids,
            reference_window,
            &mut claimed,
        ) {
            Resolution::Movable(planned) => moves.push(planned),
            Resolution::Skipped(result) => skipped.push(result),
        }
    }
    (moves, skipped)
}

/// What one target resolved to.
///
/// An `enum` rather than a `Result` because a skip is not an error: it is one of
/// the two normal outcomes of resolving a target, and both halves are reported.
enum Resolution {
    /// Resolved to a single window that can be written.
    Movable(PlannedMove),
    /// Will not be touched, with the reason to report.
    Skipped(TargetResult),
}

/// Classifies one target into a [`Resolution`].
fn plan_target(
    index: usize,
    target: &RegisteredWindow,
    windows: &HashMap<u32, Vec<OsWindow>>,
    app_pids: &HashMap<u32, u32>,
    reference_window: Option<WindowId>,
    claimed: &mut HashSet<WindowId>,
) -> Resolution {
    let key = target.key.as_str();
    let skip = |outcome, detail: String| {
        Resolution::Skipped(TargetResult {
            index,
            key: key.to_string(),
            title: target.title.clone(),
            outcome,
            detail,
            frame: None,
        })
    };

    if !target.live {
        return skip(
            OUTCOME_NO_WINDOW,
            "no window is open on this worktree any more".to_string(),
        );
    }
    let Some(title) = target.title.as_deref().filter(|t| !t.trim().is_empty()) else {
        return skip(
            OUTCOME_NO_TITLE,
            "the window reported no name to match on".to_string(),
        );
    };
    let Some(pid) = target.pid else {
        return skip(
            OUTCOME_NO_PID,
            "the window reported no process id".to_string(),
        );
    };
    let Some(app_pid) = app_pids.get(&pid).copied() else {
        return skip(
            OUTCOME_NOT_FOUND,
            format!("cannot resolve the application owning process {pid}"),
        );
    };
    let candidates = windows.get(&app_pid).map_or(&[][..], Vec::as_slice);

    // No focus tie-break for a target: the focused window is the one the user
    // invoked from, so allowing it here could only ever steal the reference.
    let window_index = match match_window(candidates, title, false) {
        Match::One(index) => index,
        Match::None => {
            return skip(
                OUTCOME_NOT_FOUND,
                format!("no window named “{title}” is open in its application"),
            )
        }
        Match::Many(count) => {
            return skip(
                OUTCOME_AMBIGUOUS,
                format!("{count} open windows match “{title}”; refusing to guess"),
            )
        }
    };
    let id = WindowId {
        app_pid,
        index: window_index,
    };
    if !claimed.insert(id) {
        // The window is spoken for. Either it is the reference — which never
        // moves, the common case when the invoking window is part of the
        // selection — or an earlier target already resolved to it, meaning one of
        // the two matches must be wrong. Both refuse, with different reasons.
        return if reference_window == Some(id) {
            skip(
                OUTCOME_REFERENCE,
                "this is the window the command was invoked from".to_string(),
            )
        } else {
            skip(
                OUTCOME_AMBIGUOUS,
                format!("“{title}” resolves to a window an earlier entry already claimed"),
            )
        };
    }
    let window = &candidates[window_index];
    if window.minimized {
        return skip(OUTCOME_MINIMIZED, "the window is minimized".to_string());
    }
    if window.fullscreen {
        return skip(
            OUTCOME_FULLSCREEN,
            "the window is in fullscreen, where a resize has no effect".to_string(),
        );
    }
    Resolution::Movable(PlannedMove {
        index,
        key: key.to_string(),
        title: title.to_string(),
        window: id,
        from: window.frame,
    })
}

/// How many standard windows a root name resolved to.
#[derive(Debug, PartialEq, Eq)]
enum Match {
    /// Exactly one, at this index.
    One(usize),
    /// None at all.
    None,
    /// This many, with no rule able to separate them.
    Many(usize),
}

/// Matches a VS Code root name against an application's window titles.
///
/// VS Code's default macOS title is the active editor, a separator, then the root
/// name — with an optional dirty marker in front and, on a non-default profile, a
/// profile suffix behind. So no single rule is both safe and sufficient, and a
/// bare `contains` is outright wrong: the root name `omni-dev` is a substring of a
/// window named `… — issue-198-omni-dev-coverage-check`.
///
/// Hence a cascade of progressively looser rules, each requiring **uniqueness**
/// before it is accepted:
///
/// 1. the title *is* the root name (a window with no editor open);
/// 2. the title *ends with* the root name (the default title format);
/// 3. the title merely *contains* it (a profile or other suffix follows).
///
/// If a level matches several windows the next level is still tried, since a looser
/// rule can happen to be more discriminating — but if none of them isolates a
/// single window the result is [`Match::Many`] and the caller refuses to guess.
/// `allow_focus_tiebreak` adds a final rule, for the reference only: among the
/// tightest ambiguous set, the application's focused window wins.
///
/// Non-standard windows (sheets, dialogs, palettes) are never candidates.
fn match_window(windows: &[OsWindow], root_name: &str, allow_focus_tiebreak: bool) -> Match {
    let standard: Vec<usize> = (0..windows.len())
        .filter(|i| windows[*i].standard)
        .collect();

    // Annotated as fn pointers so the three closures share one array type.
    let rules: [fn(&str, &str) -> bool; 3] = [
        |title, name| title == name,
        |title, name| title.ends_with(name),
        |title, name| title.contains(name),
    ];
    let mut tightest_ambiguous: Option<Vec<usize>> = None;
    for rule in rules {
        let hits: Vec<usize> = standard
            .iter()
            .copied()
            .filter(|i| rule(&windows[*i].title, root_name))
            .collect();
        match hits.len() {
            0 => {}
            1 => return Match::One(hits[0]),
            _ => {
                if tightest_ambiguous.is_none() {
                    tightest_ambiguous = Some(hits);
                }
            }
        }
    }

    match tightest_ambiguous {
        None => Match::None,
        Some(hits) => {
            if allow_focus_tiebreak {
                let focused: Vec<usize> = hits
                    .iter()
                    .copied()
                    .filter(|i| windows[*i].focused)
                    .collect();
                if let [only] = focused[..] {
                    return Match::One(only);
                }
            }
            Match::Many(hits.len())
        }
    }
}

/// Turns the platform write outcome for one planned move into its report.
///
/// `actual` is the frame read back **after** the write, so a window that clamped
/// itself to a minimum size is reported [`OUTCOME_PARTIAL`] with where it really
/// landed rather than being claimed as moved.
fn move_result(
    planned: &PlannedMove,
    wanted: Frame,
    actual: Result<Frame, String>,
) -> TargetResult {
    match actual {
        Ok(actual) if actual.matches(wanted) => {
            TargetResult::resolved(planned, OUTCOME_MOVED, "moved into position", Some(actual))
        }
        Ok(actual) => TargetResult::resolved(
            planned,
            OUTCOME_PARTIAL,
            format!(
                "settled at {}×{} at ({}, {}) instead of {}×{} at ({}, {})",
                actual.width.round(),
                actual.height.round(),
                actual.x.round(),
                actual.y.round(),
                wanted.width.round(),
                wanted.height.round(),
                wanted.x.round(),
                wanted.y.round(),
            ),
            Some(actual),
        ),
        Err(err) => TargetResult::resolved(planned, OUTCOME_FAILED, err, None),
    }
}

/// Sorts results back into the order the caller listed its targets, so a summary
/// reads in selection order rather than in whatever order planning produced.
fn in_request_order(mut results: Vec<TargetResult>) -> Vec<TargetResult> {
    results.sort_by_key(|r| r.index);
    results
}

/// Parses `ps -o pid=,ppid= -p …` output into a `pid -> ppid` map.
///
/// The op's only process introspection, deliberately a shell-out rather than more
/// FFI: an extension-host pid's parent is the VS Code main process that owns every
/// `NSWindow`. Unparseable lines are skipped rather than failing the batch — a pid
/// that has since exited simply gets no entry, which its target reports as
/// not-found. The `ps` invocation itself lives with its caller in [`ax`].
pub(super) fn parse_ppids(stdout: &str) -> HashMap<u32, u32> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn frame(x: f64, y: f64, width: f64, height: f64) -> Frame {
        Frame {
            x,
            y,
            width,
            height,
        }
    }

    fn win(title: &str, frame: Frame) -> OsWindow {
        OsWindow {
            title: title.to_string(),
            frame,
            minimized: false,
            fullscreen: false,
            standard: true,
            focused: false,
        }
    }

    fn registered(key: &str, title: &str, pid: u32) -> RegisteredWindow {
        RegisteredWindow {
            key: key.to_string(),
            live: true,
            title: Some(title.to_string()),
            pid: Some(pid),
        }
    }

    /// A backend over a fixed window table, recording every write it is asked to
    /// perform so a test can assert that a dry run performs none.
    struct FakeBackend {
        trusted: bool,
        app_pids: HashMap<u32, u32>,
        windows: HashMap<u32, Vec<OsWindow>>,
        /// Applications whose enumeration fails, and why.
        broken: HashMap<u32, String>,
        writes: RefCell<Vec<(WindowId, Frame)>>,
        /// What `set_frame` reports back instead of the requested frame, if any —
        /// how a clamped window is simulated.
        settles_at: Option<Frame>,
    }

    impl FakeBackend {
        /// Two ext-host pids (11, 12) under one application (900), whose windows
        /// carry default-format VS Code titles.
        fn new() -> Self {
            Self {
                trusted: true,
                app_pids: HashMap::from([(11, 900), (12, 900), (13, 900)]),
                windows: HashMap::from([(
                    900,
                    vec![
                        win("plan.md — reference-tree", frame(0.0, 0.0, 800.0, 600.0)),
                        win("main.rs — target-tree", frame(100.0, 200.0, 500.0, 400.0)),
                    ],
                )]),
                broken: HashMap::new(),
                writes: RefCell::new(Vec::new()),
                settles_at: None,
            }
        }

        fn writes(&self) -> Vec<(WindowId, Frame)> {
            self.writes.borrow().clone()
        }
    }

    impl WindowBackend for FakeBackend {
        fn trusted(&self) -> bool {
            self.trusted
        }

        fn app_pids(&self, pids: &[u32]) -> HashMap<u32, u32> {
            pids.iter()
                .filter_map(|p| self.app_pids.get(p).map(|app| (*p, *app)))
                .collect()
        }

        fn windows(&self, app_pid: u32) -> Result<Vec<OsWindow>, String> {
            if let Some(err) = self.broken.get(&app_pid) {
                return Err(err.clone());
            }
            Ok(self.windows.get(&app_pid).cloned().unwrap_or_default())
        }

        fn set_frame(&self, id: WindowId, frame: Frame) -> Result<Frame, String> {
            self.writes.borrow_mut().push((id, frame));
            Ok(self.settles_at.unwrap_or(frame))
        }
    }

    #[test]
    fn moves_a_target_onto_the_reference_frame() {
        let backend = FakeBackend::new();
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[registered("t1", "target-tree", 12)],
            false,
        );
        assert!(report.trusted);
        assert!(report.blocked.is_none());
        assert_eq!(
            report.reference.as_ref().unwrap().frame,
            frame(0.0, 0.0, 800.0, 600.0)
        );
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].outcome, OUTCOME_MOVED);
        assert_eq!(report.moved(), 1);
        assert_eq!(report.skipped(), 0);
        assert_eq!(
            backend.writes(),
            vec![(
                WindowId {
                    app_pid: 900,
                    index: 1
                },
                frame(0.0, 0.0, 800.0, 600.0)
            )]
        );
        assert_eq!(
            report.undo,
            vec![("t1".to_string(), frame(100.0, 200.0, 500.0, 400.0))],
            "the undo entry records where the window was before the move"
        );
    }

    #[test]
    fn a_dry_run_resolves_everything_and_writes_nothing() {
        let backend = FakeBackend::new();
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[registered("t1", "target-tree", 12)],
            true,
        );
        assert_eq!(report.results[0].outcome, OUTCOME_WOULD_MOVE);
        assert!(
            backend.writes().is_empty(),
            "a dry run must not touch a window"
        );
        assert!(report.undo.is_empty(), "a dry run leaves nothing to undo");
    }

    #[test]
    fn an_untrusted_daemon_attempts_nothing() {
        let backend = FakeBackend {
            trusted: false,
            ..FakeBackend::new()
        };
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[registered("t1", "target-tree", 12)],
            false,
        );
        assert!(!report.trusted);
        assert!(report.results.is_empty());
        assert!(report.reference.is_none());
        assert!(backend.writes().is_empty());
    }

    #[test]
    fn a_target_already_in_position_is_unchanged_not_written() {
        let mut backend = FakeBackend::new();
        // Within MATCH_EPSILON of the reference on every edge.
        backend.windows.insert(
            900,
            vec![
                win("a.rs — reference-tree", frame(10.0, 20.0, 800.0, 600.0)),
                win("b.rs — target-tree", frame(10.4, 20.4, 800.4, 600.4)),
            ],
        );
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[registered("t1", "target-tree", 12)],
            false,
        );
        assert_eq!(report.results[0].outcome, OUTCOME_UNCHANGED);
        assert!(backend.writes().is_empty());
        assert!(report.undo.is_empty());
    }

    #[test]
    fn a_clamped_window_is_partial_and_still_undoable() {
        let backend = FakeBackend {
            settles_at: Some(frame(0.0, 0.0, 640.0, 600.0)),
            ..FakeBackend::new()
        };
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[registered("t1", "target-tree", 12)],
            false,
        );
        assert_eq!(report.results[0].outcome, OUTCOME_PARTIAL);
        assert!(report.results[0].detail.contains("640"));
        assert_eq!(report.moved(), 1, "a partial move still moved the window");
        assert_eq!(report.undo.len(), 1, "and still needs undoing");
    }

    #[test]
    fn a_failed_write_is_reported_and_not_undoable() {
        struct Failing(FakeBackend);
        impl WindowBackend for Failing {
            fn trusted(&self) -> bool {
                true
            }
            fn app_pids(&self, pids: &[u32]) -> HashMap<u32, u32> {
                self.0.app_pids(pids)
            }
            fn windows(&self, app_pid: u32) -> Result<Vec<OsWindow>, String> {
                self.0.windows(app_pid)
            }
            fn set_frame(&self, _id: WindowId, _frame: Frame) -> Result<Frame, String> {
                Err("AXError -25204 (cannot complete)".to_string())
            }
        }
        let report = reposition(
            &Failing(FakeBackend::new()),
            &registered("ref", "reference-tree", 11),
            &[registered("t1", "target-tree", 12)],
            false,
        );
        assert_eq!(report.results[0].outcome, OUTCOME_FAILED);
        assert!(report.results[0].detail.contains("-25204"));
        assert_eq!(report.moved(), 0);
        assert!(report.undo.is_empty());
    }

    #[test]
    fn the_reference_window_is_never_moved_even_when_selected() {
        let backend = FakeBackend::new();
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[
                registered("ref", "reference-tree", 11),
                registered("t1", "target-tree", 12),
            ],
            false,
        );
        let outcomes: Vec<&str> = report.results.iter().map(|r| r.outcome).collect();
        assert_eq!(
            outcomes,
            vec![OUTCOME_REFERENCE, OUTCOME_MOVED],
            "results stay in request order"
        );
        assert_eq!(backend.writes().len(), 1);
    }

    #[test]
    fn two_targets_cannot_claim_the_same_window() {
        let backend = FakeBackend::new();
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[
                registered("t1", "target-tree", 12),
                registered("t2", "target-tree", 13),
            ],
            false,
        );
        let outcomes: Vec<&str> = report.results.iter().map(|r| r.outcome).collect();
        assert_eq!(
            outcomes,
            vec![OUTCOME_MOVED, OUTCOME_AMBIGUOUS],
            "a duplicate claim is an ambiguity, not the reference"
        );
    }

    #[test]
    fn minimized_and_fullscreen_targets_are_skipped() {
        let mut backend = FakeBackend::new();
        backend.windows.insert(
            900,
            vec![
                win("a — reference-tree", frame(0.0, 0.0, 800.0, 600.0)),
                OsWindow {
                    minimized: true,
                    ..win("b — dozing-tree", frame(0.0, 0.0, 10.0, 10.0))
                },
                OsWindow {
                    fullscreen: true,
                    ..win("c — huge-tree", frame(0.0, 0.0, 2560.0, 1440.0))
                },
            ],
        );
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[
                registered("t1", "dozing-tree", 12),
                registered("t2", "huge-tree", 13),
            ],
            false,
        );
        let outcomes: Vec<&str> = report.results.iter().map(|r| r.outcome).collect();
        assert_eq!(outcomes, vec![OUTCOME_MINIMIZED, OUTCOME_FULLSCREEN]);
        assert!(backend.writes().is_empty());
    }

    #[test]
    fn missing_title_pid_and_window_each_get_their_own_outcome() {
        let backend = FakeBackend::new();
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[
                RegisteredWindow {
                    key: "blank".to_string(),
                    live: true,
                    title: Some("   ".to_string()),
                    pid: Some(12),
                },
                RegisteredWindow {
                    key: "pidless".to_string(),
                    live: true,
                    title: Some("target-tree".to_string()),
                    pid: None,
                },
                registered("stranger", "target-tree", 99),
                registered("gone", "never-opened", 12),
            ],
            false,
        );
        let outcomes: Vec<&str> = report.results.iter().map(|r| r.outcome).collect();
        assert_eq!(
            outcomes,
            vec![
                OUTCOME_NO_TITLE,
                OUTCOME_NO_PID,
                OUTCOME_NOT_FOUND,
                OUTCOME_NOT_FOUND
            ],
            "an unresolvable pid and an unmatched name are both not-found"
        );
    }

    #[test]
    fn an_unenumerable_application_leaves_its_targets_not_found() {
        let mut backend = FakeBackend::new();
        backend
            .broken
            .insert(900, "AXError -25211 (API disabled)".to_string());
        let report = reposition(
            &backend,
            &registered("ref", "reference-tree", 11),
            &[registered("t1", "target-tree", 12)],
            false,
        );
        // With no windows at all the reference cannot resolve, so the batch is
        // blocked rather than half-applied.
        assert_eq!(report.blocked.as_ref().unwrap().reason, BLOCKED_NOT_FOUND);
        assert!(report.results.is_empty());
    }

    #[test]
    fn an_unresolvable_reference_blocks_the_whole_batch() {
        let backend = FakeBackend::new();
        for (reference, expected) in [
            (
                RegisteredWindow {
                    key: "r".to_string(),
                    live: true,
                    title: None,
                    pid: Some(11),
                },
                BLOCKED_UNIDENTIFIABLE,
            ),
            (
                RegisteredWindow {
                    key: "r".to_string(),
                    live: true,
                    title: Some("reference-tree".to_string()),
                    pid: None,
                },
                BLOCKED_UNIDENTIFIABLE,
            ),
            (
                registered("r", "reference-tree", 99),
                BLOCKED_UNIDENTIFIABLE,
            ),
            (registered("r", "no-such-window", 11), BLOCKED_NOT_FOUND),
        ] {
            let report = reposition(
                &backend,
                &reference,
                &[registered("t1", "target-tree", 12)],
                false,
            );
            assert_eq!(report.blocked.as_ref().unwrap().reason, expected);
            assert!(report.results.is_empty(), "no target is attempted");
            assert!(backend.writes().is_empty());
        }
    }

    #[test]
    fn an_ambiguous_reference_blocks_the_batch() {
        let mut backend = FakeBackend::new();
        backend.windows.insert(
            900,
            vec![
                win("a.rs — main", frame(0.0, 0.0, 10.0, 10.0)),
                win("b.rs — main", frame(0.0, 0.0, 20.0, 20.0)),
            ],
        );
        let report = reposition(&backend, &registered("r", "main", 11), &[], false);
        assert_eq!(report.blocked.as_ref().unwrap().reason, BLOCKED_AMBIGUOUS);
    }

    #[test]
    fn a_fullscreen_or_minimized_reference_blocks_the_batch() {
        for (fullscreen, expected) in [(true, BLOCKED_FULLSCREEN), (false, BLOCKED_MINIMIZED)] {
            let mut backend = FakeBackend::new();
            backend.windows.insert(
                900,
                vec![OsWindow {
                    fullscreen,
                    minimized: !fullscreen,
                    ..win("a — reference-tree", frame(0.0, 0.0, 800.0, 600.0))
                }],
            );
            let report = reposition(&backend, &registered("r", "reference-tree", 11), &[], false);
            assert_eq!(report.blocked.as_ref().unwrap().reason, expected);
        }
    }

    #[test]
    fn restore_puts_each_window_back_at_its_own_frame() {
        let backend = FakeBackend::new();
        let entries = vec![
            (
                registered("t1", "target-tree", 12),
                frame(300.0, 400.0, 700.0, 500.0),
            ),
            // The reference window is just another target on the undo path — there
            // is no reference to exclude.
            (
                registered("ref", "reference-tree", 11),
                frame(50.0, 60.0, 900.0, 700.0),
            ),
        ];
        let report = restore(&backend, &entries);
        assert!(report.reference.is_none());
        let outcomes: Vec<&str> = report.results.iter().map(|r| r.outcome).collect();
        assert_eq!(outcomes, vec![OUTCOME_MOVED, OUTCOME_MOVED]);
        assert_eq!(
            backend.writes(),
            vec![
                (
                    WindowId {
                        app_pid: 900,
                        index: 1
                    },
                    frame(300.0, 400.0, 700.0, 500.0)
                ),
                (
                    WindowId {
                        app_pid: 900,
                        index: 0
                    },
                    frame(50.0, 60.0, 900.0, 700.0)
                ),
            ]
        );
    }

    #[test]
    fn restore_reports_a_window_that_has_since_closed() {
        let backend = FakeBackend::new();
        let entries = vec![(
            registered("gone", "closed-since", 12),
            frame(0.0, 0.0, 10.0, 10.0),
        )];
        let report = restore(&backend, &entries);
        assert_eq!(report.results[0].outcome, OUTCOME_NOT_FOUND);
        assert!(backend.writes().is_empty());
    }

    #[test]
    fn ends_with_beats_a_substring_collision() {
        // The real collision from the live setup: the root name `omni-dev` is a
        // substring of another worktree's name, so `contains` alone is ambiguous.
        let windows = [
            win("lib.rs — omni-dev", frame(0.0, 0.0, 10.0, 10.0)),
            win(
                "x.rs — issue-198-omni-dev-coverage-check",
                frame(0.0, 0.0, 10.0, 10.0),
            ),
        ];
        assert_eq!(match_window(&windows, "omni-dev", false), Match::One(0));
        assert_eq!(
            match_window(&windows, "issue-198-omni-dev-coverage-check", false),
            Match::One(1)
        );
    }

    #[test]
    fn a_bare_root_name_title_wins_over_a_decorated_one() {
        // One window has no editor open, so its title *is* the root name.
        let windows = [
            win("some-tree", frame(0.0, 0.0, 10.0, 10.0)),
            win("a.rs — other-some-tree", frame(0.0, 0.0, 10.0, 10.0)),
        ];
        assert_eq!(match_window(&windows, "some-tree", false), Match::One(0));
    }

    #[test]
    fn a_profile_suffix_still_matches_by_substring() {
        let windows = [win("a.rs — some-tree — Work", frame(0.0, 0.0, 10.0, 10.0))];
        assert_eq!(match_window(&windows, "some-tree", false), Match::One(0));
    }

    #[test]
    fn a_dirty_marker_prefix_does_not_break_the_match() {
        let windows = [win("● a.rs — some-tree", frame(0.0, 0.0, 10.0, 10.0))];
        assert_eq!(match_window(&windows, "some-tree", false), Match::One(0));
    }

    #[test]
    fn duplicate_root_names_are_ambiguous_not_guessed() {
        let windows = [
            win("a.rs — main", frame(0.0, 0.0, 10.0, 10.0)),
            win("b.rs — main", frame(0.0, 0.0, 10.0, 10.0)),
        ];
        assert_eq!(match_window(&windows, "main", false), Match::Many(2));
    }

    #[test]
    fn focus_breaks_a_tie_for_the_reference_only() {
        let windows = [
            win("a.rs — main", frame(0.0, 0.0, 10.0, 10.0)),
            OsWindow {
                focused: true,
                ..win("b.rs — main", frame(0.0, 0.0, 10.0, 10.0))
            },
        ];
        assert_eq!(match_window(&windows, "main", true), Match::One(1));
        assert_eq!(
            match_window(&windows, "main", false),
            Match::Many(2),
            "a target must never win on focus — that is the reference's window"
        );
    }

    #[test]
    fn non_standard_windows_are_never_candidates() {
        let windows = [OsWindow {
            standard: false,
            ..win("Open — some-tree", frame(0.0, 0.0, 10.0, 10.0))
        }];
        assert_eq!(match_window(&windows, "some-tree", true), Match::None);
    }

    #[test]
    fn parses_ps_output() {
        // The real shape of `ps -o pid=,ppid= -p …`: right-aligned, leading spaces.
        let out = "  28112  65488\n  28113  65488\n  65488      1\n";
        let map = parse_ppids(out);
        assert_eq!(map.get(&28112).copied(), Some(65488));
        assert_eq!(map.get(&65488).copied(), Some(1));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn ignores_unparseable_ps_lines() {
        let out = "\n  ps: 999: no such process\n  10  20\nnotanumber x\n";
        assert_eq!(parse_ppids(out), HashMap::from([(10, 20)]));
    }
}
