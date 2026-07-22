//! The extensible omni-dev daemon: a long-lived supervisor that hosts pluggable
//! [`DaemonService`](service::DaemonService)s over a local Unix-domain control
//! socket.
//!
//! The daemon owns **lifecycle, single-instance supervision, status
//! aggregation, and (on macOS) the menu-bar shell**. Each service wraps its own
//! work and exposes status/control; the browser bridge is the first such
//! service (#987). The control socket is a private operator/tray channel — it
//! does **not** carry any service's own data plane (e.g. the bridge keeps its
//! loopback-TCP planes per ADR-0036). See ADR-0039.
//!
//! Process model:
//! - `daemon run` *becomes* the daemon ([`server::run`]), blocking until a
//!   signal or a built-in `shutdown` op.
//! - `daemon start` launches it in the background (a launchd LaunchAgent on
//!   macOS, a systemd user unit on Linux); `stop` / `restart` / `status` are thin
//!   [`client::DaemonClient`]s.

// The control plane is a Unix-domain socket (`UnixListener`/`UnixStream`), so the
// daemon runtime is Unix-only and gated `#[cfg(unix)]`; on Windows it runs only
// under WSL2 (a real Linux kernel), and a native (non-WSL) Windows port is future
// work (#1363). `paths` stays cross-platform because the request log and the
// browser thin-client token discovery depend on it.
pub mod paths;

#[cfg(unix)]
pub mod client;
#[cfg(unix)]
pub mod lifecycle;
#[cfg(unix)]
pub mod protocol;
#[cfg(unix)]
pub mod registry;
#[cfg(unix)]
pub mod selection;
#[cfg(unix)]
pub mod server;
#[cfg(unix)]
pub mod service;
#[cfg(unix)]
pub mod services;
#[cfg(unix)]
pub mod single_instance;

#[cfg(all(unix, test))]
pub(crate) mod testutil;

#[cfg(target_os = "macos")]
pub mod launchd;

#[cfg(target_os = "linux")]
pub mod systemd;

#[cfg(all(target_os = "macos", feature = "menu-bar"))]
pub mod tray;

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use anyhow::Result;

#[cfg(unix)]
use crate::browser::BridgeConfig;
#[cfg(unix)]
use crate::snowflake::SnowflakeEngineConfig;
#[cfg(unix)]
use registry::ServiceRegistry;
#[cfg(unix)]
pub use selection::{DaemonServiceKind, ServiceSelection};
#[cfg(unix)]
use server::DaemonOptions;
#[cfg(unix)]
use services::bridge::BridgeService;
#[cfg(unix)]
use services::github_counters::GithubCountersService;
#[cfg(unix)]
use services::sessions::SessionsService;
#[cfg(unix)]
use services::snowflake::SnowflakeService;
#[cfg(unix)]
use services::worktrees::WorktreesService;

/// Everything `daemon run` needs to start the daemon, resolved from the CLI.
///
/// Shared by the headless path ([`run_headless`]) and the macOS menu-bar path
/// (`tray::run`) so both start an identical daemon. The latter is a plain code
/// span, not an intra-doc link, because the `tray` module is feature- and
/// target-gated and absent from the docs build.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct DaemonRunConfig {
    /// Control-socket path (also the single-instance lock).
    pub socket_path: PathBuf,
    /// Browser-bridge configuration (ports, allow-origin, limits).
    pub bridge_config: BridgeConfig,
    /// Optional file the bridge session token is read from instead of generated.
    pub bridge_token_file: Option<PathBuf>,
    /// Where the resolved bridge token is persisted (`0600`) for thin clients.
    pub bridge_token_path: PathBuf,
    /// Which default-registry services to host (all, or an explicit subset).
    pub services: ServiceSelection,
}

/// Builds the daemon's default service registry.
///
/// Starts the browser bridge on its loopback-TCP planes and registers it
/// alongside the Snowflake query service, the cross-window worktrees registry,
/// and the Claude Code sessions tracker.
///
/// `bridge_token_file` overrides token generation; `bridge_token_path` is where
/// the resolved token is persisted (`0600`) for thin-client discovery. The
/// Snowflake service is registered cheaply (no eager auth or I/O); its sessions
/// are authenticated lazily on first query. The worktrees and sessions services
/// are likewise cheap (in-memory only); they fill as VS Code windows register and
/// as Claude Code hooks/transcripts report.
///
/// `services` selects which of the four to host. A service outside the selection
/// is never constructed, so its startup work is skipped entirely — no bridge TCP
/// planes, no worktrees pollers, no sessions watcher (#1318). The default
/// ([`ServiceSelection::All`]) hosts everything.
#[cfg(unix)]
pub async fn build_default_registry(
    bridge_config: BridgeConfig,
    bridge_token_file: Option<&Path>,
    bridge_token_path: PathBuf,
    services: &ServiceSelection,
) -> Result<ServiceRegistry> {
    let mut registry = ServiceRegistry::new();
    if services.includes(DaemonServiceKind::Bridge) {
        let bridge =
            BridgeService::start(bridge_config, bridge_token_file, bridge_token_path).await?;
        registry.register(Arc::new(bridge));
    }
    if services.includes(DaemonServiceKind::Snowflake) {
        let snowflake = SnowflakeService::new(SnowflakeEngineConfig::from_env_and_settings()?);
        registry.register(Arc::new(snowflake));
    }
    if services.includes(DaemonServiceKind::Worktrees) {
        // Start the off-thread menu-refresh loop so the tray serves a cached menu
        // instead of running git enrichment on the macOS GUI thread (#1186 fix).
        let worktrees = WorktreesService::new();
        // Seed the per-repo PR-poll enable set from its persisted `0600` file so the
        // user's choices survive a restart; the poller reads it below (#1376). A path
        // that cannot be resolved (no data dir) just disables persistence.
        match crate::daemon::paths::worktrees_polling_path() {
            Ok(path) => worktrees.load_polling_prefs(path),
            Err(err) => tracing::warn!("worktrees polling prefs disabled: {err:#}"),
        }
        // Seed the resolved PR-badge cache from its persisted `0600` file so a restart
        // serves badges instantly and the poller can skip its immediate re-poll when
        // they are still fresh (#1389, fix 4). Before `start_pr_poller` so the warm
        // start is in place when the loop spawns; a path that cannot be resolved just
        // disables persistence.
        match crate::daemon::paths::worktrees_pr_cache_path() {
            Ok(path) => worktrees.load_pr_cache(path),
            Err(err) => tracing::warn!("worktrees PR cache disabled: {err:#}"),
        }
        worktrees.start_menu_refresh();
        // Keep PR check badges fresh for every open window from one `gh` call, rather
        // than each window resolving its own and none of them ever re-asking (#1337).
        worktrees.start_pr_poller();
        // Watch the GitHub API budget the PR poller (and every other `gh` on the box)
        // spends, so `daemon status` / the tray surface an approaching exhaustion before
        // it rate-limits everything — polling `/rate_limit` is exempt, so this is free
        // (#1375). Share the cache with the registry for the built-in `status` op.
        worktrees.start_rate_limit_poller();
        let rate_limit_cache = worktrees.rate_limit_cache();
        registry.register(Arc::new(worktrees));
        registry.set_github_rate_limit(rate_limit_cache);
    }
    if services.includes(DaemonServiceKind::Sessions) {
        // The cross-window Claude Code sessions tracker; start its transcript watcher
        // (Feed 2) so sessions predating the daemon — and the hook-silent thinking
        // window — are still tracked (#1210).
        let sessions = SessionsService::new();
        sessions.start_watcher();
        registry.register(Arc::new(sessions));
    }
    // Periodically log a summary of the GitHub API-call counters (#1387): once
    // ~5s after boot, every 10 minutes, and once on shutdown. Best-effort and
    // bounded (a small local log read, no network); never blocks shutdown. Not one
    // of the selectable services (#1318) — a daemon-wide observability concern that
    // stays on for any subset.
    let github_counters = GithubCountersService::new();
    github_counters.start_counter_logger();
    registry.register(Arc::new(github_counters));
    Ok(registry)
}

/// Runs the daemon headlessly (no tray).
///
/// Builds the registry and serves until a signal or `daemon stop`. The default
/// `daemon run` path on every platform, and the only path when the `menu-bar`
/// feature is off.
#[cfg(unix)]
pub async fn run_headless(cfg: DaemonRunConfig) -> Result<()> {
    let registry = build_default_registry(
        cfg.bridge_config,
        cfg.bridge_token_file.as_deref(),
        cfg.bridge_token_path,
        &cfg.services,
    )
    .await?;
    server::run(
        registry,
        DaemonOptions {
            socket_path: cfg.socket_path,
        },
    )
    .await
}

#[cfg(all(unix, test))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A subset selection registers only the chosen services and, crucially,
    /// never constructs the others — so their startup work does not run. Snowflake
    /// is the cheap probe here: its `new()` does no eager auth or I/O, and gating
    /// out the bridge means the test binds no TCP planes. See #1318.
    #[tokio::test]
    async fn build_default_registry_honours_a_subset() {
        let selection = ServiceSelection::Only(vec![DaemonServiceKind::Snowflake]);
        let registry = build_default_registry(
            BridgeConfig::default(),
            None,
            PathBuf::from("/nonexistent/bridge.token"),
            &selection,
        )
        .await
        .expect("a snowflake-only registry builds without touching the bridge");

        // Only the selected service is constructed, plus the always-on GitHub
        // API-call counter logger (#1387) — daemon-wide observability that is not
        // one of the four selectable kinds, so it rides along in any subset.
        let names: Vec<_> = registry.services().iter().map(|s| s.name()).collect();
        assert_eq!(names, vec![services::snowflake::SERVICE_NAME, "github"]);
    }
}
