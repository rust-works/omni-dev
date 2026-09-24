//! `omni-dev codex-wrap` — the Codex TUI on a private app-server, observed.
//!
//! Runs the Codex TUI against a **private** Codex app-server that this process
//! starts and owns, and reports the exact thread status it polls from that
//! server to the daemon's `sessions` service (#1910, ADR-0088). The Codex
//! analogue of [`omni-dev claude-wrap`](super::claude_wrap).
//!
//! The Codex VS Code extension and Desktop each run their own private stdio
//! app-server, so no outside process can read their threads' status. A session
//! omni-dev launches can be different: this wrapper starts
//! `codex app-server --listen unix://<runtime-dir>/codex-wrap-<pid>.sock`, runs
//! `codex --remote unix://… <args>` in the foreground on the caller's terminal,
//! and polls that server with [`crate::sessions::codex_app_server`] — never
//! subscribing and never answering a server request, so it cannot see, answer
//! or duplicate an approval. It attaches only to the server it started.
//!
//! **Fail-open is the hard rule**, as for `claude-wrap`: if the app-server does
//! not come up, this process is replaced by plain `codex <args>`; if the
//! observer cannot connect, the TUI still runs, unobserved. An invocation the
//! remote TUI does not serve (a non-interactive subcommand, no terminal, or an
//! explicit `--remote`) is passed straight to `codex` the same way.
//!
//! The server lives exactly as long as the wrapper: when the TUI exits, the
//! wrapper ends the sessions it reported, stops the server and removes its
//! socket. Nothing is logged or persisted.

use std::io::IsTerminal;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Parser;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::daemon::client::DaemonClient;
use crate::daemon::paths;
use crate::daemon::protocol::DaemonEnvelope;
use crate::daemon::server;
use crate::sessions::codex_app_server::{AppServerClient, Report, StatusTracker};

/// The `sessions` service routing key on the daemon control socket.
const SERVICE: &str = "sessions";

/// How long a fire-and-forget report waits for the daemon.
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the server is polled for thread status.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long connecting to the server (socket, WebSocket upgrade, `initialize`)
/// may take before the wrapper gives up observing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive failed polls after which the server is taken to be gone.
const MAX_POLL_FAILURES: u32 = 3;

/// How long the app-server may take to start listening before the wrapper
/// gives up on it and runs plain `codex` instead.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the app-server gets to exit on `SIGTERM` before it is killed.
const SERVER_STOP_GRACE: Duration = Duration::from_secs(2);

/// Exit status for a child killed by a signal (`128 + signo`).
const SIGNAL_EXIT_BASE: i32 = 128;

/// `codex` subcommands that are not the interactive TUI, so `--remote` does not
/// apply and the wrapper passes them straight through. `resume` and `fork` open
/// the TUI and are wrapped.
const PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "agents",
    "app",
    "app-server",
    "apply",
    "a",
    "archive",
    "cloud",
    "completion",
    "debug",
    "delete",
    "doctor",
    "e",
    "exec",
    "exec-server",
    "features",
    "help",
    "login",
    "logout",
    "mcp",
    "migrate-rollouts",
    "plugin",
    "remote-control",
    "review",
    "sandbox",
    "unarchive",
    "update",
];

/// Runs the Codex TUI against a private app-server, reporting its exact state.
#[derive(Parser)]
pub struct CodexWrapCommand {
    /// Path to the daemon control socket. Defaults to the standard per-user path.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// The Codex command and its arguments, after a `--` separator
    /// (e.g. `-- codex`, `-- codex resume <id>`).
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD"
    )]
    pub argv: Vec<String>,
}

impl CodexWrapCommand {
    /// Executes the wrapper and exits with the TUI's own status — like
    /// `claude-wrap`, via [`std::process::exit`], so a wrapped run writes no
    /// request-log record.
    pub async fn execute(self) -> Result<()> {
        let code = self.run().await?;
        std::process::exit(code)
    }

    async fn run(self) -> Result<i32> {
        let Some((program, args)) = self.argv.split_first() else {
            bail!("codex-wrap needs a command to run, e.g. `omni-dev codex-wrap -- codex`");
        };
        if !std::io::stdout().is_terminal() || !is_tui_invocation(args) {
            return Err(exec_replace(program, args));
        }
        let Some(listen) = listen_path() else {
            return Err(exec_replace(program, args));
        };
        let Some(server) = start_server(program, &listen).await else {
            return Err(exec_replace(program, args));
        };
        Ok(wrap(program, args, &listen, server, self.socket).await)
    }
}

/// Whether `args` (after the `codex` program) open the interactive TUI the
/// `--remote` flag serves: no pass-through subcommand anywhere among the
/// positionals, and no `--remote` of the caller's own.
fn is_tui_invocation(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        arg == "--remote"
            || arg.starts_with("--remote=")
            || PASSTHROUGH_SUBCOMMANDS.contains(&arg.as_str())
    })
}

/// The private server's socket: `<runtime-dir>/codex-wrap-<pid>.sock`, in the
/// daemon's `0700` directory. `None` (so the wrapper passes through) when the
/// directory cannot be made or the path is too long for a `sockaddr_un`.
fn listen_path() -> Option<PathBuf> {
    let dir = paths::runtime_dir().ok()?;
    paths::ensure_dir_0700(&dir).ok()?;
    let path = dir.join(format!("codex-wrap-{}.sock", std::process::id()));
    (path.as_os_str().len() < paths::MAX_SOCKET_PATH_LEN).then_some(path)
}

/// Starts `<program> app-server --listen unix://<listen>` and waits for it to
/// accept connections. `None` if it exits or does not come up in time — the
/// caller then runs plain `codex`.
async fn start_server(program: &str, listen: &Path) -> Option<Child> {
    let _ = std::fs::remove_file(listen);
    let mut child = Command::new(program)
        .arg("app-server")
        .arg("--listen")
        .arg(format!("unix://{}", listen.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let deadline = tokio::time::Instant::now() + SERVER_READY_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::UnixStream::connect(listen).await.is_ok() {
            return Some(child);
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop_server(&mut child, listen).await;
    None
}

/// Runs the TUI against the server, observes until it exits, then cleans up.
/// Returns the TUI's exit code.
async fn wrap(
    program: &str,
    args: &[String],
    listen: &Path,
    mut server: Child,
    socket: Option<PathBuf>,
) -> i32 {
    let tui = std::process::Command::new(program)
        .arg("--remote")
        .arg(format!("unix://{}", listen.display()))
        .args(args)
        .spawn();
    let Ok(mut tui) = tui else {
        stop_server(&mut server, listen).await;
        // The same program just failed to spawn; report it as the shell would.
        return 127;
    };
    let stop = CancellationToken::new();
    let observer = tokio::spawn(observe(listen.to_path_buf(), socket, stop.clone()));
    let signals = tokio::spawn(forward_signals(tui.id()));
    // The TUI owns the terminal; wait for it off the async runtime.
    let status = tokio::task::spawn_blocking(move || tui.wait())
        .await
        .ok()
        .and_then(Result::ok);
    signals.abort();
    stop.cancel();
    let _ = observer.await;
    stop_server(&mut server, listen).await;
    status.map_or(1, exit_code)
}

/// Polls the server, re-asserting every live session's state each poll, and
/// ends every reported session once `stop` fires or the server stops
/// answering. Connecting and each poll race `stop`, so a stalled server can
/// never keep the wrapper from exiting.
async fn observe(listen: PathBuf, socket: Option<PathBuf>, stop: CancellationToken) {
    let Ok(socket) = server::resolve_socket(socket) else {
        return;
    };
    let connect = tokio::time::timeout(CONNECT_TIMEOUT, AppServerClient::connect(&listen));
    let mut client = tokio::select! {
        () = stop.cancelled() => return,
        connected = connect => match connected {
            Ok(Ok(client)) => client,
            _ => return,
        },
    };
    let mut tracker = StatusTracker::default();
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    let mut failures = 0;
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            _ = poll.tick() => {}
        }
        let polled = tokio::select! {
            () = stop.cancelled() => break,
            polled = client.snapshot() => polled,
        };
        if let Ok(snapshot) = polled {
            failures = 0;
            send(&socket, tracker.update(&snapshot)).await;
        } else {
            // A closed connection fails every call at once, so a few in a row
            // means the server is gone (the TUI will follow it); one slow poll
            // does not.
            failures += 1;
            if failures >= MAX_POLL_FAILURES {
                break;
            }
        }
    }
    send(&socket, tracker.finish()).await;
}

/// Sends reports to the daemon, fire-and-forget: a missing or wedged daemon is
/// a silent no-op.
async fn send(socket: &Path, reports: Vec<Report>) {
    for report in reports {
        let (op, payload): (&str, Value) = match report {
            Report::Observe(request) => match serde_json::to_value(request) {
                Ok(payload) => ("observe", payload),
                Err(_) => continue,
            },
            Report::End(id) => ("end", json!({ "session_id": id })),
        };
        let envelope = DaemonEnvelope::service(SERVICE, op, payload);
        let _ =
            tokio::time::timeout(REPORT_TIMEOUT, DaemonClient::new(socket).request(envelope)).await;
    }
}

/// Stops the server — `SIGTERM`, then a kill after [`SERVER_STOP_GRACE`] — and
/// removes its socket. The server was started by this process and is its child,
/// so the pid is its own.
async fn stop_server(server: &mut Child, listen: &Path) {
    if let Some(pid) = server.id().and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    if tokio::time::timeout(SERVER_STOP_GRACE, server.wait())
        .await
        .is_err()
    {
        let _ = server.kill().await;
    }
    let _ = std::fs::remove_file(listen);
}

/// Relays `SIGTERM`/`SIGHUP` to the TUI, so a caller that signals the wrapper
/// stops Codex rather than orphaning it, and swallows `SIGINT` — the TUI shares
/// the terminal's process group and gets it directly; the wrapper must survive
/// to clean up.
async fn forward_signals(child_pid: u32) {
    use nix::sys::signal::Signal;
    use tokio::signal::unix::{signal, SignalKind};

    let Ok(pid) = i32::try_from(child_pid) else {
        return;
    };
    let pid = nix::unistd::Pid::from_raw(pid);
    let (Ok(mut terminate), Ok(mut hangup), Ok(mut interrupt)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
        signal(SignalKind::interrupt()),
    ) else {
        return;
    };
    loop {
        let relay = tokio::select! {
            _ = terminate.recv() => Some(Signal::SIGTERM),
            _ = hangup.recv() => Some(Signal::SIGHUP),
            _ = interrupt.recv() => None,
        };
        if let Some(signal) = relay {
            let _ = nix::sys::signal::kill(pid, signal);
        }
    }
}

/// Replaces this process with `program`, returning the error only if the `exec`
/// itself failed.
fn exec_replace(program: &str, args: &[String]) -> anyhow::Error {
    let error = std::process::Command::new(program).args(args).exec();
    anyhow::Error::new(error).context(format!("failed to exec {program}"))
}

/// The exit code to leave with: the TUI's own, or `128 + signo`.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT_BASE + signal))
        .unwrap_or(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn only_tui_invocations_are_wrapped() {
        assert!(is_tui_invocation(&args(&[])));
        assert!(is_tui_invocation(&args(&["fix the build"])));
        assert!(is_tui_invocation(&args(&[
            "-m",
            "gpt-5.5",
            "--no-alt-screen"
        ])));
        assert!(is_tui_invocation(&args(&["resume", "019a-id"])));
        assert!(is_tui_invocation(&args(&["fork", "--last"])));
        assert!(!is_tui_invocation(&args(&["exec", "say hi"])));
        assert!(!is_tui_invocation(&args(&["-c", "x=1", "login"])));
        assert!(!is_tui_invocation(&args(&["--remote", "ws://h:1"])));
        assert!(!is_tui_invocation(&args(&["--remote=ws://h:1"])));
    }

    #[test]
    fn codex_wrap_parses_its_trailing_command() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            cmd: CodexWrapCommand,
        }
        let parsed = Wrapper::parse_from(["x", "--", "codex", "resume", "--last"]);
        assert_eq!(parsed.cmd.argv, args(&["codex", "resume", "--last"]));
        assert!(parsed.cmd.socket.is_none());
    }

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(ExitStatus::from_raw(9)), SIGNAL_EXIT_BASE + 9);
    }

    #[tokio::test]
    async fn a_server_that_never_listens_is_abandoned_for_plain_codex() {
        // `false app-server …` exits at once without listening.
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let listen = tmp.path().join("as.sock");
        assert!(start_server("false", &listen).await.is_none());
        assert!(!listen.exists());
        // A program that cannot even be spawned is the same pass-through.
        assert!(start_server("/nonexistent/codex", &listen).await.is_none());
    }

    #[tokio::test]
    async fn a_run_without_a_command_is_an_error() {
        let cmd = CodexWrapCommand {
            socket: None,
            argv: Vec::new(),
        };
        let err = cmd.run().await.unwrap_err();
        assert!(err.to_string().contains("needs a command"), "{err}");
    }
}
