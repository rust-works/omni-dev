//! PTY spawn and lifecycle for one embedded terminal tab (issue #1585
//! Phase 3).
//!
//! `alacritty_terminal`'s own `tty` + `EventLoop` do all the I/O: one native
//! OS thread per live tab runs the crate's poll loop over the PTY fd and
//! parses bytes into the shared `Term` *on that thread*. This module only
//! wires that thread up and forwards its events to the async side over one
//! shared channel ([`UiEventProxy`]) — there is no per-tab async task.
//!
//! Two spike-proven requirements are load-bearing here (issue #1585 §5b):
//!
//! 1. **`TERM`/`COLORTERM` are set explicitly per spawn** ([`child_env`]).
//!    `tty::Options.env` is applied *additively* over the inherited
//!    environment, so a parent with no `TERM` would otherwise spawn a child
//!    that silently degrades to dumb-terminal rendering (`vim` never enters
//!    the alt screen). They are set per spawn rather than via the crate's
//!    process-wide `tty::setup_env()`, which mutates *this* process's
//!    environment through an `unsafe` `set_var`.
//! 2. **Every `Event::PtyWrite` must be written back to the child.** The
//!    emulator answers cursor-position / device-attribute queries itself and
//!    hands the reply back as an event rather than writing it anywhere; a
//!    real interactive child blocks waiting for that reply. The write-back
//!    lives in [`super::TerminalTab::handle_event`]; the test below pins it
//!    against a real child.
//!
//! **Security:** PTY bytes may carry credentials. Nothing in this module or
//! its callers logs, traces, or persists PTY traffic, `PtyWrite` contents,
//! selection text, or clipboard text — pinned by the
//! `no_pty_content_is_ever_logged` guard test in `super`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;
use anyhow::{Context, Result};
use tokio::sync::mpsc;

/// Identifies one tab across the shared event channel.
pub type TabId = u64;

/// The `WindowSize` pixel dimensions reported to the child. Nothing here
/// renders pixels, but a child that computes an image-protocol cell size from
/// `ws_xpixel / ws_col` gets a plausible answer rather than `0`.
const CELL_WIDTH_PX: u16 = 8;
const CELL_HEIGHT_PX: u16 = 16;

/// A `(columns, lines)` pair for `Term::new`/`Term::resize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub lines: u16,
}

impl GridSize {
    pub fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.lines,
            num_cols: self.cols,
            cell_width: CELL_WIDTH_PX,
            cell_height: CELL_HEIGHT_PX,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        usize::from(self.lines)
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.lines)
    }

    fn columns(&self) -> usize {
        usize::from(self.cols)
    }
}

/// The emulator's [`EventListener`]: a sync callback invoked on the PTY
/// thread that pushes `(tab, event)` onto the one shared channel the async
/// event loop drains. `UnboundedSender::send` never blocks and needs no
/// runtime, which is what makes it safe to call from a plain OS thread.
#[derive(Clone)]
pub struct UiEventProxy {
    tab: TabId,
    tx: mpsc::UnboundedSender<(TabId, TermEvent)>,
}

impl EventListener for UiEventProxy {
    fn send_event(&self, event: TermEvent) {
        // A closed receiver means the UI is gone; dropping the event is the
        // only sensible outcome and the PTY thread will be shut down shortly.
        let _ = self.tx.send((self.tab, event));
    }
}

/// What to run in the new PTY.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub tab: TabId,
    /// `None` runs the user's default shell (the crate resolves `$SHELL`,
    /// and on macOS goes through `/usr/bin/login` so it is a real login
    /// session); `Some((program, args))` runs that instead.
    pub program: Option<(String, Vec<String>)>,
    pub cwd: PathBuf,
    pub size: GridSize,
    pub extra_env: Vec<(String, String)>,
}

/// Everything a live tab holds onto: the shared emulator state, the channel
/// into the PTY thread, and the thread itself (joined on shutdown).
pub struct PtyHandle {
    pub term: Arc<FairMutex<Term<UiEventProxy>>>,
    pub sender: EventLoopSender,
    thread: Option<JoinHandle<(EventLoop<tty::Pty, UiEventProxy>, State)>>,
    /// The child's pid, captured at spawn.
    ///
    /// `alacritty_terminal` calls `setsid()` in the child's `pre_exec`, so
    /// the child is a session and process-group leader and its **pgid equals
    /// this pid** — which is what lets [`reap_child_group`] signal the shell
    /// *and* everything it started, rather than the shell alone.
    pub child_pid: i32,
}

impl PtyHandle {
    /// Takes the PTY thread's join handle for the caller to `join` off the
    /// async runtime; the tuple it yields owns the `Pty`, whose `Drop` sends
    /// the child `SIGHUP` and reaps it.
    pub fn take_thread(
        &mut self,
    ) -> Option<JoinHandle<(EventLoop<tty::Pty, UiEventProxy>, State)>> {
        self.thread.take()
    }
}

/// How long a child is given to honour `SIGHUP` before it is killed.
pub const REAP_GRACE: Duration = Duration::from_millis(250);

/// Ends the child's whole process group: `SIGHUP`, a grace period, then
/// `SIGKILL` unconditionally.
///
/// **Why this exists (#1605).** Dropping `tty::Pty` sends the *direct child*
/// `SIGHUP` and then blocks in `child.wait()` with no deadline. A shell that
/// ignores or blocks the signal — or one still waiting on a foreground job of
/// its own — hangs that wait forever. The blocking join keeps it off the
/// event loop, but that was never the whole problem: dropping the `tokio`
/// runtime waits for blocking tasks, so `worktrees ui` could restore the
/// terminal, hand back the prompt, and then never exit.
///
/// Two properties make this a fix rather than a workaround. It signals the
/// **process group**, so a shell's own children go too — `alacritty_terminal`
/// calls `setsid()` in the child's `pre_exec`, making its pgid its pid, while
/// the `Pty` drop reaches only the shell. (Same approach as
/// `claude::ai::claude_cli::kill_and_reap`.) And it **escalates**, so a
/// well-behaved child still exits on `SIGHUP` with its `drain_on_exit` output
/// intact; only one that would otherwise hang us is killed.
///
/// Two things it deliberately does **not** do, both learned the hard way:
///
/// * It does not poll the group for liveness. A dead-but-unreaped child is a
///   zombie, and a zombie still answers `kill(pid, 0)` on Linux while macOS
///   reports it gone — so a liveness poll cannot tell "exited" from "ignoring
///   us", and would burn the whole grace on every clean exit on one platform
///   while short-circuiting on the other.
/// * It does not hand the escalation to a watchdog thread that races the
///   join. Unblocking a stuck wait must not itself depend on winning a race
///   under load.
///
/// So the sequence is unconditional and synchronous, and the caller runs it
/// before the join. `SIGKILL` to an already-exited child is `ESRCH`, a no-op:
/// a child that honoured `SIGHUP` is still reported as dying of `SIGHUP`.
/// The cost is that shutdown always spends the grace period on a plain OS
/// thread, detached from the async runtime, which is the price of being
/// deterministic on every platform.
#[cfg(unix)]
pub fn reap_child_group(pid: i32, grace: Duration) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    // Never signal pid 0 (our own group) or init. `kill(-1, …)` means every
    // process we may signal, so negatives are refused outright rather than
    // merely being unlikely.
    if pid <= 1 {
        return;
    }
    let group = Pid::from_raw(pid);
    let _ = killpg(group, Signal::SIGHUP);
    std::thread::sleep(grace);
    let _ = killpg(group, Signal::SIGKILL);
}

/// No-op off unix: there is no process group to signal, and the Windows
/// build never reaches the `SIGHUP` path this exists to bound.
#[cfg(not(unix))]
pub fn reap_child_group(_pid: i32, _grace: Duration) {}

/// The child's extra environment: `TERM`/`COLORTERM` always, plus `extra`.
/// `xterm-256color` is what every terminfo database ships, which is why it
/// is preferred over probing for an `alacritty` entry the way the crate's
/// own `setup_env` does.
pub fn child_env(extra: impl IntoIterator<Item = (String, String)>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());
    env.extend(extra);
    env
}

/// Spawns the child in a fresh PTY and starts its reader thread.
pub fn spawn(
    request: &SpawnRequest,
    tx: mpsc::UnboundedSender<(TabId, TermEvent)>,
) -> Result<PtyHandle> {
    let proxy = UiEventProxy {
        tab: request.tab,
        tx,
    };
    // `..Default::default()` is needless on unix (every field is named) but
    // required on Windows, where `Options` has an extra `escape_args` field.
    #[allow(clippy::needless_update)]
    let options = tty::Options {
        shell: request
            .program
            .as_ref()
            .map(|(program, args)| tty::Shell::new(program.clone(), args.clone())),
        working_directory: Some(request.cwd.clone()),
        // Read whatever the child wrote right before exiting, so its last
        // lines land in the grid instead of being lost with the fd.
        drain_on_exit: true,
        env: child_env(request.extra_env.iter().cloned()),
        ..Default::default()
    };
    let pty = tty::new(&options, request.size.window_size(), request.tab)
        .with_context(|| format!("failed to spawn a terminal in {}", request.cwd.display()))?;
    // Captured before the `EventLoop` takes ownership of the `Pty` — this is
    // the only point the pid is reachable.
    // 0 on the (impossible) conversion failure, never -1: `kill(-1, …)`
    // means "every process we may signal", so a negative sentinel sitting
    // next to a signalling API is not worth the risk even behind a guard.
    // PIDs fit in i32 in practice (Linux caps at ~2^22, macOS at 99999).
    let child_pid = i32::try_from(pty.child().id()).unwrap_or(0);

    // `kitty_keyboard` stays off (the `Config` default): the emulator then
    // never acknowledges a child's kitty-keyboard-protocol query, so the
    // child keeps sending/expecting legacy xterm sequences — which is all
    // `super::super::keys` encodes this phase. Advertising the protocol is a
    // deliberate follow-up, not an oversight.
    let term = Term::new(Config::default(), &request.size, proxy.clone());
    let term = Arc::new(FairMutex::new(term));
    let event_loop = EventLoop::new(Arc::clone(&term), proxy, pty, options.drain_on_exit, false)
        .context("failed to start the PTY reader")?;
    let sender = event_loop.channel();
    let thread = event_loop.spawn();
    Ok(PtyHandle {
        term,
        sender,
        thread: Some(thread),
        child_pid,
    })
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod reap_tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use super::*;

    /// `process_group(0)` mirrors the `setsid()` a real PTY spawn does, so
    /// the group-wide signal under test is the one production sends.
    fn group_leader(script: &str) -> std::process::Child {
        let child = Command::new("/bin/sh")
            .args(["-c", script])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Let the shell install any trap before it is signalled.
        std::thread::sleep(Duration::from_millis(100));
        child
    }

    fn sigkill() -> i32 {
        nix::sys::signal::Signal::SIGKILL as i32
    }

    fn sighup() -> i32 {
        nix::sys::signal::Signal::SIGHUP as i32
    }

    /// #1605, the whole fix: a child that cannot be `SIGHUP`ed away is still
    /// gone within the grace period, so the wait it would have blocked
    /// forever returns.
    ///
    /// `trap '' HUP` makes the signal ignored *and* inherited as ignored, so
    /// nothing but the escalation can end this.
    #[test]
    fn a_child_ignoring_sighup_is_escalated_and_reaped() {
        let mut child = group_leader("trap '' HUP; sleep 60");
        let pid = i32::try_from(child.id()).unwrap();
        let started = Instant::now();
        reap_child_group(pid, REAP_GRACE);
        // Stands in for the join the real caller makes: it returns only once
        // the child is actually gone.
        let status = child.wait().unwrap();

        assert_eq!(
            status.signal(),
            Some(sigkill()),
            "a child ignoring SIGHUP must be escalated to SIGKILL"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "reaping took {:?}; it is meant to be bounded by the grace period",
            started.elapsed()
        );
    }

    /// A well-behaved child dies of `SIGHUP` and is **never** escalated —
    /// asserted on the exit signal, not on elapsed time, so it cannot be
    /// marginal. This is the case the earlier group-liveness poll got wrong:
    /// a zombie still answers `kill(pid, 0)` on Linux, so polling the group
    /// burned the whole grace period and escalated even here.
    #[test]
    fn a_well_behaved_child_dies_of_sighup_and_is_never_escalated() {
        let mut child = group_leader("sleep 60");
        let pid = i32::try_from(child.id()).unwrap();
        reap_child_group(pid, REAP_GRACE);
        let status = child.wait().unwrap();

        assert_eq!(
            status.signal(),
            Some(sighup()),
            "a child that honours SIGHUP must die of it, not of SIGKILL"
        );
    }

    /// The pid captured from a **real PTY spawn** is the group leader we can
    /// signal — the assumption the whole fix rests on, and the one the
    /// `Command`-based tests above cannot check, since they build the group
    /// themselves with `process_group(0)` rather than relying on the
    /// `setsid()` inside `alacritty_terminal`'s spawn.
    #[test]
    fn a_pty_spawned_child_is_a_signallable_group_leader() {
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;

        let (tx, _rx) = mpsc::unbounded_channel();
        let request = SpawnRequest {
            tab: 99,
            program: Some((
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "sleep 30".to_string()],
            )),
            cwd: std::env::temp_dir(),
            size: GridSize { cols: 40, lines: 6 },
            extra_env: Vec::new(),
        };
        let handle = spawn(&request, tx).unwrap();
        let pid = handle.child_pid;
        assert!(pid > 1, "spawn must capture a real pid, got {pid}");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            killpg(Pid::from_raw(pid), None).is_ok(),
            "the captured pid is not a process-group leader, so a group-wide \
             signal would miss the child entirely"
        );
        reap_child_group(pid, Duration::from_millis(50));
        assert_eq!(
            killpg(Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH),
            "the group survived reaping"
        );
    }

    /// Signalling our own group, init, or "every process we may signal"
    /// would each be catastrophic, so all three are refused outright.
    #[test]
    fn pid_zero_init_and_negatives_are_never_signalled() {
        for pid in [0, 1, -1, -5] {
            reap_child_group(pid, Duration::from_millis(1));
        }
        // Reaching here is the assertion: this process and its group — which
        // include the test runner — are still alive.
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::borrow::Cow;
    use std::time::Duration;

    use alacritty_terminal::event_loop::Msg;
    use alacritty_terminal::term::TermMode;

    use super::*;

    fn sh(script: &str) -> SpawnRequest {
        SpawnRequest {
            tab: 1,
            program: Some((
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )),
            cwd: std::env::temp_dir(),
            size: GridSize {
                cols: 60,
                lines: 10,
            },
            extra_env: Vec::new(),
        }
    }

    /// The visible grid as one string per line, trailing spaces trimmed.
    fn grid_text(handle: &PtyHandle) -> String {
        let term = handle.term.lock();
        let cols = term.columns();
        let mut out = String::new();
        let mut line = String::new();
        for cell in term.grid().display_iter() {
            line.push(cell.c);
            if cell.point.column.0 + 1 == cols {
                out.push_str(line.trim_end());
                out.push('\n');
                line.clear();
            }
        }
        out
    }

    /// Drains events until `done` returns true or `timeout` elapses;
    /// returns every event seen. Never logs event contents.
    async fn drain_until(
        rx: &mut mpsc::UnboundedReceiver<(TabId, TermEvent)>,
        handle: &PtyHandle,
        timeout: Duration,
        mut done: impl FnMut(&TermEvent, &PtyHandle) -> bool,
    ) -> Vec<TermEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return seen;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some((_, event))) => {
                    // Mirror the app: answer the emulator's own queries, or a
                    // querying child would hang exactly as §5b describes.
                    if let TermEvent::PtyWrite(reply) = &event {
                        let _ = handle
                            .sender
                            .send(Msg::Input(Cow::Owned(reply.as_bytes().to_vec())));
                    }
                    let finished = done(&event, handle);
                    seen.push(event);
                    if finished {
                        return seen;
                    }
                }
                Ok(None) | Err(_) => return seen,
            }
        }
    }

    fn shutdown(mut handle: PtyHandle) {
        let _ = handle.sender.send(Msg::Shutdown);
        if let Some(thread) = handle.take_thread() {
            let _ = thread.join();
        }
    }

    #[test]
    fn child_env_always_sets_term_and_colorterm() {
        let env = child_env(std::iter::empty());
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));

        let env = child_env([("FOO".to_string(), "bar".to_string())]);
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert!(env.contains_key("TERM"));
    }

    #[tokio::test]
    async fn echo_round_trip_lands_in_the_grid_and_reports_child_exit() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn(&sh("echo hello-from-pty"), tx).unwrap();
        let events = drain_until(&mut rx, &handle, Duration::from_secs(10), |e, _| {
            matches!(e, TermEvent::Exit)
        })
        .await;
        assert!(
            events.iter().any(|e| matches!(e, TermEvent::ChildExit(_))),
            "expected a ChildExit event"
        );
        assert!(
            grid_text(&handle).contains("hello-from-pty"),
            "grid was:\n{}",
            grid_text(&handle)
        );
        shutdown(handle);
    }

    /// The §5b regression test: a child that queries the terminal (`CSI 6n`,
    /// cursor position) and then blocks reading the reply only proceeds if
    /// the emulator's `PtyWrite` answer is written back to it.
    #[tokio::test]
    async fn pty_write_replies_are_echoed_back_so_a_querying_child_does_not_hang() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let script = "stty -echo -icanon min 1 time 0 2>/dev/null; \
                      printf '\\033[6n'; \
                      c=$(dd bs=1 count=1 2>/dev/null); \
                      if [ -n \"$c\" ]; then echo GOT-REPLY; else echo NO-REPLY; fi";
        let handle = spawn(&sh(script), tx).unwrap();
        let events = drain_until(&mut rx, &handle, Duration::from_secs(10), |e, _| {
            matches!(e, TermEvent::Exit)
        })
        .await;
        assert!(
            events.iter().any(|e| matches!(e, TermEvent::PtyWrite(_))),
            "the child's CSI 6n never produced a PtyWrite — the query path itself is broken"
        );
        let text = grid_text(&handle);
        assert!(text.contains("GOT-REPLY"), "grid was:\n{text}");
        shutdown(handle);
    }

    /// The other §5b regression test: with `TERM` set explicitly per spawn,
    /// a real full-screen program enters the alternate screen and leaves it
    /// again on exit. (Without `TERM` it silently never would.)
    #[tokio::test]
    async fn vim_enters_and_leaves_the_alt_screen_with_term_set_explicitly() {
        let Some(vim) = which("vim") else {
            eprintln!("skipping: no `vim` on PATH");
            return;
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let request = SpawnRequest {
            program: Some((
                vim,
                vec!["-u".into(), "NONE".into(), "-i".into(), "NONE".into()],
            )),
            ..sh("")
        };
        let handle = spawn(&request, tx).unwrap();

        drain_until(&mut rx, &handle, Duration::from_secs(10), |_, h| {
            h.term.lock().mode().contains(TermMode::ALT_SCREEN)
        })
        .await;
        assert!(
            handle.term.lock().mode().contains(TermMode::ALT_SCREEN),
            "vim never entered the alt screen"
        );

        handle
            .sender
            .send(Msg::Input(Cow::Borrowed(b":q!\r")))
            .unwrap();
        drain_until(&mut rx, &handle, Duration::from_secs(10), |e, _| {
            matches!(e, TermEvent::Exit)
        })
        .await;
        assert!(
            !handle.term.lock().mode().contains(TermMode::ALT_SCREEN),
            "vim left without restoring the primary screen"
        );
        shutdown(handle);
    }

    #[tokio::test]
    async fn osc52_from_a_child_arrives_as_a_clipboard_store_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // `aGVsbG8=` is base64("hello").
        let handle = spawn(&sh("printf '\\033]52;c;aGVsbG8=\\a'"), tx).unwrap();
        let events = drain_until(&mut rx, &handle, Duration::from_secs(10), |e, _| {
            matches!(e, TermEvent::ClipboardStore(..))
        })
        .await;
        let stored = events.iter().find_map(|e| match e {
            TermEvent::ClipboardStore(_, text) => Some(text.clone()),
            _ => None,
        });
        assert_eq!(stored.as_deref(), Some("hello"));
        shutdown(handle);
    }

    fn which(program: &str) -> Option<String> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    }
}
