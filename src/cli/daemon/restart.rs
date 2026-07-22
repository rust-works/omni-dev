//! `omni-dev daemon restart` — stop then start the daemon.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::time::timeout;

use super::control;
use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::StatusReport;
use crate::daemon::{server, DaemonServiceKind, ServiceSelection};

/// Upper bound on the pre-shutdown `status` probe. `status` fans out to every
/// service's `status().await` (worktrees enriches inline via a full `git_status`)
/// and has no client-side timeout of its own, so a wedged service must not hang
/// the restart here. Generous enough for normal git enrichment across many
/// worktrees; on timeout the restart proceeds without preserving the selection
/// (#1352 review).
const STATUS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Restarts the daemon: stop it (if running), then start it again.
#[derive(Parser)]
pub struct RestartCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Re-bake this comma-separated subset of services (default: preserve
    /// whatever the running daemon currently hosts). Values: browser-bridge,
    /// snowflake, worktrees, sessions.
    #[arg(long, value_name = "SVC", value_delimiter = ',')]
    pub services: Vec<DaemonServiceKind>,
}

impl RestartCommand {
    /// Executes the restart command.
    pub async fn execute(self) -> Result<()> {
        let socket_path = server::resolve_socket(self.socket)?;
        let client = DaemonClient::new(&socket_path);
        // Read the current status *before* shutdown so an omitted `--services`
        // can preserve the running daemon's selection (a restart re-writes the
        // plist/unit, so it can't just leave the baked args alone). A `status`
        // reply also stands in for the old `ping` liveness check.
        //
        // Bound the probe (#1352 review): `status` aggregates every service's
        // `status().await`, so a wedged service could otherwise hang the restart
        // here. On timeout we treat the daemon as running-but-opaque — still stop
        // and relaunch it, just without a selection to preserve (an omitted
        // `--services` then falls back to `All`).
        // `.ok()` folds a timeout (`Elapsed`) into `None`; a live-or-down `status`
        // stays `Some(Ok | Err)`. `classify_probe` turns that into the restart
        // decision.
        let probe = timeout(STATUS_PROBE_TIMEOUT, client.status()).await.ok();
        if probe.is_none() {
            tracing::warn!(
                "daemon status probe timed out after {STATUS_PROBE_TIMEOUT:?}; \
                 restarting without preserving its current service selection"
            );
        }
        let (is_running, running) = classify_probe(probe);
        if is_running {
            client.shutdown().await.ok();
            // On the socket-activated Linux path, pinging the still-armed systemd
            // socket would re-activate the daemon; the relaunch + readiness ping
            // below drive a clean handoff instead (systemd serializes at most one
            // service instance, so the old drains and a fresh one comes up). Safe
            // on the detached-spawn fallback too, where `bind_or_reclaim` handles
            // the brief socket contention. (#1174)
            #[cfg(not(target_os = "linux"))]
            control::wait_until_down(&socket_path).await?;
        }
        let services = resolve_services(&self.services, running.as_ref());
        // On macOS `launch` re-bootstraps via `install_and_load`, which already
        // boots out any prior agent before bootstrapping. Do *not* boot out
        // separately first: that would unregister auto-start in a window where a
        // failed/aborted re-bootstrap leaves the daemon both stopped and
        // unregistered — strictly worse than before `restart` ran. See #994.
        control::launch(&socket_path, &services)?;
        control::wait_until_ready(&socket_path).await?;
        println!("daemon restarted (socket {})", socket_path.display());
        Ok(())
    }
}

/// Classifies the bounded pre-shutdown probe into `(is_running, report)`.
///
/// The input is the `status` result with a timeout already folded in via `.ok()`:
/// `Some(Ok)` is a live daemon whose selection we read; `Some(Err)` means nothing
/// answered on the socket (already down); `None` means the probe *timed out* (a
/// wedged service). A timeout is treated as running-but-opaque — the restart still
/// stops and relaunches the daemon, just with no selection to preserve, so an
/// omitted `--services` falls back to [`ServiceSelection::All`]. Pure, so the
/// three-way decision is unit-tested without a live socket (#1352 review).
fn classify_probe(probe: Option<Result<StatusReport>>) -> (bool, Option<StatusReport>) {
    match probe {
        Some(Ok(report)) => (true, Some(report)),
        Some(Err(_)) => (false, None),
        None => (true, None),
    }
}

/// Resolves the service selection to re-bake on restart. An explicit
/// `--services` flag wins; otherwise the running daemon's current services are
/// preserved; a daemon that was already down (no status) falls back to hosting
/// everything. The shell env var is deliberately *not* consulted — restart
/// mirrors what is live, and a set change is made explicit via the flag.
fn resolve_services(
    flag: &[DaemonServiceKind],
    running: Option<&StatusReport>,
) -> ServiceSelection {
    if !flag.is_empty() {
        return ServiceSelection::resolve(flag, None);
    }
    match running {
        Some(report) => {
            ServiceSelection::from_service_names(report.services.iter().map(|s| s.name.as_str()))
        }
        None => ServiceSelection::All,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::service::ServiceStatus;

    fn report(names: &[&str]) -> StatusReport {
        StatusReport {
            services: names
                .iter()
                .map(|n| ServiceStatus {
                    name: (*n).to_string(),
                    healthy: true,
                    summary: String::new(),
                    detail: serde_json::Value::Null,
                })
                .collect(),
            version: None,
            ..StatusReport::default()
        }
    }

    #[test]
    fn classify_probe_reads_a_live_daemons_selection() {
        // `Some(Ok)` — a daemon answered `status`; it is running and its report is
        // preserved so an omitted `--services` re-bakes its current subset.
        let (is_running, running) = classify_probe(Some(Ok(report(&["worktrees"]))));
        assert!(is_running);
        assert_eq!(
            resolve_services(&[], running.as_ref()),
            ServiceSelection::Only(vec![DaemonServiceKind::Worktrees])
        );
    }

    #[test]
    fn classify_probe_treats_a_socket_error_as_down() {
        // `Some(Err)` — nothing answered on the socket; not running, no report, so
        // there is no shutdown to attempt and the bake falls back to `All`.
        let (is_running, running) = classify_probe(Some(Err(anyhow::anyhow!("refused"))));
        assert!(!is_running);
        assert!(running.is_none());
        assert_eq!(
            resolve_services(&[], running.as_ref()),
            ServiceSelection::All
        );
    }

    #[test]
    fn classify_probe_treats_a_timeout_as_running_but_opaque() {
        // `None` — the probe timed out on a wedged service; the daemon is still
        // running (so the restart stops+relaunches it) but its selection is
        // unreadable, so an omitted `--services` falls back to `All` (#1352 review).
        let (is_running, running) = classify_probe(None);
        assert!(is_running);
        assert!(running.is_none());
        assert_eq!(
            resolve_services(&[], running.as_ref()),
            ServiceSelection::All
        );
    }

    #[test]
    fn an_explicit_flag_wins_over_the_running_selection() {
        let selection = resolve_services(
            &[DaemonServiceKind::Worktrees],
            Some(&report(&["browser-bridge", "snowflake"])),
        );
        assert_eq!(
            selection,
            ServiceSelection::Only(vec![DaemonServiceKind::Worktrees])
        );
    }

    #[test]
    fn an_omitted_flag_preserves_the_running_selection() {
        let selection = resolve_services(&[], Some(&report(&["worktrees", "sessions"])));
        assert_eq!(
            selection,
            ServiceSelection::Only(vec![
                DaemonServiceKind::Worktrees,
                DaemonServiceKind::Sessions
            ])
        );
    }

    #[test]
    fn a_down_daemon_with_no_flag_falls_back_to_all() {
        assert_eq!(resolve_services(&[], None), ServiceSelection::All);
    }

    #[test]
    fn a_full_daemon_restarts_as_all_not_a_frozen_explicit_list() {
        // A daemon hosting every selectable service (its `status` also lists the
        // always-on `github` observability service) must resolve back to `All`, so
        // the re-baked plist/unit stays byte-identical to a fresh `start` and a
        // future selectable service still auto-enables on a plain restart rather
        // than being frozen out of the baked list (#1352 review).
        let selection = resolve_services(
            &[],
            Some(&report(&[
                "browser-bridge",
                "snowflake",
                "worktrees",
                "sessions",
                "github",
            ])),
        );
        assert_eq!(selection, ServiceSelection::All);
    }
}
