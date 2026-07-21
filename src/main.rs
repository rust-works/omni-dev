use std::process;
use std::time::Instant;

use clap::{CommandFactory, Parser};
use omni_dev::request_log::{self, InvocationOutcome, RequestLogContext, Source};
use omni_dev::Cli;

fn main() {
    // Capture argv before clap consumes it, so the invocation record can log the
    // full command line and the resolved subcommand path — and so tracing can be
    // initialized at the right default level for the resolved command *before* any
    // log line is emitted.
    let argv: Vec<String> = std::env::args().collect();
    let command = resolve_command_path(&argv);
    let daemon_run = is_daemon_run(&command);

    // The long-lived `daemon run` defaults to `info` so its lifecycle events reach
    // the log sink; short-lived CLI invocations stay at `warn`. `RUST_LOG` still
    // overrides either. See #1316.
    init_tracing(daemon_run);

    let cli = Cli::parse();

    // Install the per-invocation context up front — crucially *before* the macOS
    // menu-bar handoff below, which `return`s without ever reaching the common
    // path. Otherwise a tray-hosted daemon's `gh`/HTTP records would default to
    // `Source::Cli` instead of `Daemon` (#1387). `daemon run` → Daemon, else Cli;
    // set_global is first-write-wins.
    let source = if daemon_run {
        Source::Daemon
    } else {
        Source::Cli
    };
    request_log::set_global(RequestLogContext {
        invocation_id: request_log::new_id(),
        source,
        mcp_tool: None,
    });

    // The macOS menu-bar daemon needs the GUI event loop on the main thread,
    // which a tokio runtime cannot own. Detect `daemon run` (without
    // `--no-menu`) and hand the main thread to the tray; every other invocation
    // runs the async CLI on a multi-thread runtime exactly as before.
    #[cfg(all(target_os = "macos", feature = "menu-bar"))]
    if let Some(run_config) = cli.menu_bar_run_config() {
        let cfg = match run_config {
            Ok(cfg) => cfg,
            Err(e) => die(&e),
        };
        match omni_dev::daemon::tray::run(cfg) {
            Ok(()) => return,
            Err(e) => die(&e),
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("Error: failed to start the tokio runtime: {e}");
            process::exit(1);
        }
    };

    // Time the whole command and append one invocation record after it returns.
    // Logging is best-effort and never affects the exit code. (The per-invocation
    // context was installed up front, above, before the menu-bar handoff.)
    let start = Instant::now();
    let result = runtime.block_on(cli.execute());

    let (exit_code, error) = match &result {
        Ok(()) => (0, None),
        Err(e) => (1, Some(format!("{e:#}"))),
    };
    request_log::record_invocation(InvocationOutcome {
        command,
        command_line: argv,
        exit_code,
        error,
        duration: start.elapsed(),
    });

    if let Err(e) = result {
        die(&e);
    }
}

/// Resolves the clap subcommand path (e.g. `["jira","read"]`) by re-deriving
/// matches from argv and walking the subcommand chain. Generic — robust to new
/// subcommands — and returns an empty path if re-parsing fails.
fn resolve_command_path(argv: &[String]) -> Vec<String> {
    let mut path = Vec::new();
    let Ok(matches) = Cli::command().try_get_matches_from(argv) else {
        return path;
    };
    let mut current = &matches;
    while let Some((name, sub)) = current.subcommand() {
        path.push(name.to_string());
        current = sub;
    }
    path
}

/// Whether the resolved command path is `daemon run` (the long-lived daemon).
fn is_daemon_run(command: &[String]) -> bool {
    command.first().map(String::as_str) == Some("daemon")
        && command.get(1).map(String::as_str) == Some("run")
}

/// The default tracing filter for a resolved command when `RUST_LOG` is unset:
/// `info` for the long-lived `daemon run` (so its lifecycle events — start/stop,
/// signals — reach the log sink), `warn` for every short-lived CLI invocation.
fn default_filter(daemon_run: bool) -> &'static str {
    if daemon_run {
        "info"
    } else {
        "warn"
    }
}

/// Initializes the tracing subscriber (stderr, `RUST_LOG`-driven), keeping
/// daemon/debug logs off stdout. The default level when `RUST_LOG` is unset is
/// [`default_filter`]; `RUST_LOG` still overrides it.
fn init_tracing(daemon_run: bool) {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter(daemon_run))),
        )
        .init();
}

/// Prints an error and its source chain to stderr, then exits non-zero.
fn die(e: &anyhow::Error) -> ! {
    eprintln!("Error: {e}");
    let mut source = e.source();
    while let Some(err) = source {
        eprintln!("  Caused by: {err}");
        source = err.source();
    }
    process::exit(1);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn is_daemon_run_matches_only_daemon_run() {
        assert!(is_daemon_run(&path(&["daemon", "run"])));
        // Trailing flags after `daemon run` still count (only the path prefix matters).
        assert!(is_daemon_run(&path(&[
            "daemon",
            "run",
            "--socket",
            "/tmp/d.sock"
        ])));
        // Other daemon subcommands are short-lived clients, not the daemon.
        assert!(!is_daemon_run(&path(&["daemon", "status"])));
        assert!(!is_daemon_run(&path(&["daemon", "start"])));
        assert!(!is_daemon_run(&path(&["daemon"])));
        assert!(!is_daemon_run(&path(&["jira", "read"])));
        assert!(!is_daemon_run(&[]));
    }

    #[test]
    fn default_filter_is_info_only_for_daemon_run() {
        assert_eq!(default_filter(true), "info");
        assert_eq!(default_filter(false), "warn");
    }
}
