use std::process;
use std::time::Instant;

use clap::{CommandFactory, Parser};
use omni_dev::request_log::{self, InvocationOutcome, RequestLogContext, Source};
use omni_dev::utils::env::{EnvSource, SystemEnv};
use omni_dev::utils::settings::Settings;
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
    // overrides either, and `daemon.log_level` in settings.json sits between the
    // two for `daemon run` specifically (issue #1447) — the only way to raise a
    // launchd/systemd-spawned daemon's level, since neither passes RUST_LOG
    // through to its environment. Scoped to `daemon_run` so every other
    // invocation (notably the latency-sensitive `sessions hook` sink) neither
    // pays for the settings-file read nor inherits a level meant for the
    // daemon. See #1316.
    let daemon_log_level = daemon_run
        .then(|| Settings::load_daemon().log_level)
        .flatten();
    init_tracing(daemon_run, daemon_log_level.as_deref());

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

/// Resolves the tracing filter directive, honouring the precedence `RUST_LOG`
/// (process env) > `settings.daemon.log_level` > [`default_filter`] (issue
/// #1447).
///
/// `daemon_log_level` is consulted only when `daemon_run` is true —
/// `daemon.log_level` is a `daemon run`-scoped setting, so a non-daemon
/// invocation must resolve as though it were unset even if the caller passes
/// one in (defense in depth; the one production call site in `main` already
/// only loads it for `daemon_run`).
///
/// Pure over an injected [`EnvSource`] so the precedence is unit-testable
/// without mutating the process environment — mirrors
/// `crate::mcp::runtime::resolve_log_directive`. Empty values at either layer
/// are ignored so a blank `RUST_LOG` or `log_level` does not mask the next
/// tier.
fn resolve_log_directive(
    env: &impl EnvSource,
    daemon_run: bool,
    daemon_log_level: Option<&str>,
) -> String {
    env.var("RUST_LOG")
        .filter(|s| !s.is_empty())
        .or_else(|| {
            daemon_run
                .then_some(daemon_log_level)
                .flatten()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| default_filter(daemon_run).to_string())
}

/// Initializes the tracing subscriber (stderr, `RUST_LOG`-driven), keeping
/// daemon/debug logs off stdout. The filter directive is resolved via
/// [`resolve_log_directive`].
fn init_tracing(daemon_run: bool, daemon_log_level: Option<&str>) {
    let directive = resolve_log_directive(&SystemEnv, daemon_run, daemon_log_level);
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(directive))
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
    use std::collections::HashMap;

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    /// A pure in-memory [`EnvSource`] for this binary crate's tests. `main.rs`
    /// is a separate crate from `omni_dev`'s lib, so it cannot reach the
    /// lib's private, `#[cfg(test)]`-only `test_support::env::MapEnv` — this
    /// is that same fake, redefined locally.
    #[derive(Default)]
    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn new() -> Self {
            Self::default()
        }

        fn with(mut self, key: &str, value: &str) -> Self {
            self.0.insert(key.to_string(), value.to_string());
            self
        }
    }

    impl EnvSource for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
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

    #[test]
    fn resolve_log_directive_rust_log_wins_over_setting() {
        let env = MapEnv::new().with("RUST_LOG", "trace");
        assert_eq!(resolve_log_directive(&env, true, Some("info")), "trace");
    }

    #[test]
    fn resolve_log_directive_setting_used_when_rust_log_unset_and_daemon_run() {
        let env = MapEnv::new();
        assert_eq!(resolve_log_directive(&env, true, Some("debug")), "debug");
    }

    #[test]
    fn resolve_log_directive_setting_ignored_when_not_daemon_run() {
        // `daemon.log_level` is scoped to `daemon run` — a non-daemon
        // invocation (e.g. `sessions hook`) must not inherit it even when
        // RUST_LOG is unset, or setting it to debug the daemon would also
        // spam every other command's stderr (issue #1447 follow-up).
        let env = MapEnv::new();
        assert_eq!(resolve_log_directive(&env, false, Some("debug")), "warn");
    }

    #[test]
    fn resolve_log_directive_falls_back_to_default_filter_per_daemon_run() {
        let env = MapEnv::new();
        assert_eq!(resolve_log_directive(&env, true, None), "info");
        assert_eq!(resolve_log_directive(&env, false, None), "warn");
    }

    #[test]
    fn resolve_log_directive_ignores_empty_values() {
        // A blank RUST_LOG falls through to the setting (when daemon_run); a
        // blank setting falls through to the daemon_run-conditioned default.
        let env = MapEnv::new().with("RUST_LOG", "");
        assert_eq!(resolve_log_directive(&env, true, Some("trace")), "trace");
        assert_eq!(resolve_log_directive(&env, true, Some("")), "info");
    }
}
