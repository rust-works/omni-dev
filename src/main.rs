use std::process;
use std::time::Instant;

use clap::{CommandFactory, Parser};
use omni_dev::request_log::{self, InvocationOutcome, RequestLogContext, Source};
use omni_dev::Cli;

fn main() {
    init_tracing();

    // Capture argv before clap consumes it, so the invocation record can log the
    // full command line and the resolved subcommand path.
    let argv: Vec<String> = std::env::args().collect();
    let cli = Cli::parse();

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

    // Stash the per-invocation context so HTTP records can correlate to this
    // run, then time the whole command and append one invocation record after
    // it returns. Logging is best-effort and never affects the exit code.
    let command = resolve_command_path(&argv);
    let source = if is_daemon_run(&command) {
        Source::Daemon
    } else {
        Source::Cli
    };
    request_log::set_global(RequestLogContext {
        invocation_id: request_log::new_id(),
        source,
        mcp_tool: None,
    });

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

/// Initializes the tracing subscriber (stderr, `RUST_LOG`-driven, default
/// `warn`), keeping daemon/debug logs off stdout.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
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
