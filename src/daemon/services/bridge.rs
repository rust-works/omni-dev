//! The browser-bridge daemon service: hosts the bridge's loopback-TCP planes
//! under the daemon's lifecycle and exposes status/control to it.
//!
//! The security model is unchanged from ADR-0036 — the bridge keeps both its
//! TCP planes and their bearer-token auth, and the daemon never proxies browser
//! traffic. The only additive delta is that the resolved session token is
//! persisted to a `0600` file so thin clients (`request`/`harvest`) can
//! discover it without the foreground stdout a standalone `serve` prints.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::browser::protocol::StatusResponse;
use crate::browser::{auth, snippet, BridgeConfig, BridgeServer};
use crate::daemon::paths;
use crate::daemon::service::{DaemonService, MenuAction, MenuItem, MenuSnapshot, ServiceStatus};

/// The browser-bridge service name (the control-socket routing key).
pub const SERVICE_NAME: &str = "browser-bridge";

/// Hosts a [`BridgeServer`] under the daemon, persisting its session token for
/// thin-client discovery and allowing in-place restart.
pub struct BridgeService {
    /// The running server; `None` only transiently during a restart.
    inner: StdMutex<Option<BridgeServer>>,
    config: BridgeConfig,
    token: Arc<String>,
    token_path: PathBuf,
}

impl BridgeService {
    /// Resolves the session token, persists it to a `0600` file, starts the
    /// bridge planes, and returns the service.
    pub async fn start(
        config: BridgeConfig,
        token_file: Option<&Path>,
        token_path: PathBuf,
    ) -> Result<Self> {
        let token = auth::resolve_token(token_file)?;
        write_token(&token_path, &token)?;
        let server = BridgeServer::start(config.clone(), token.clone()).await?;
        Ok(Self {
            inner: StdMutex::new(Some(server)),
            config,
            token: Arc::new(token),
            token_path,
        })
    }

    /// Locks the inner server slot, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, Option<BridgeServer>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stops the running bridge and starts a fresh one with the same config and
    /// token. The lock is never held across the awaits.
    async fn restart(&self) -> Result<()> {
        let old = self.lock().take();
        if let Some(server) = old {
            server.shutdown().await;
        }
        let server = BridgeServer::start(self.config.clone(), (*self.token).clone()).await?;
        *self.lock() = Some(server);
        Ok(())
    }
}

/// Writes `token` to `path` (`0600`), creating its parent directory (`0700`).
fn write_token(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        paths::ensure_dir_0700(parent)?;
    }
    std::fs::write(path, token)
        .with_context(|| format!("failed to write token file {}", path.display()))?;
    paths::set_file_0600(path)?;
    Ok(())
}

/// A one-line summary for `daemon status` / the tray.
fn summarize(status: &StatusResponse, control_port: u16, ws_port: u16) -> String {
    if status.connected {
        format!(
            "{} tab(s), {} pending (control :{control_port}, ws :{ws_port})",
            status.tabs.len(),
            status.pending
        )
    } else {
        format!("no tab connected (control :{control_port}, ws :{ws_port})")
    }
}

#[async_trait]
impl DaemonService for BridgeService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    async fn handle(&self, op: &str, payload: Value) -> Result<Value> {
        match op {
            "status" => {
                let snapshot = self.lock().as_ref().map(BridgeServer::status);
                match snapshot {
                    Some(status) => Ok(serde_json::to_value(status)?),
                    None => Ok(json!({ "running": false })),
                }
            }
            "disconnect-tab" => {
                let id = payload
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("`disconnect-tab` requires a numeric `id`"))?;
                let guard = self.lock();
                let server = guard
                    .as_ref()
                    .ok_or_else(|| anyhow!("bridge is not running"))?;
                server.disconnect_tab(id)?;
                Ok(json!({ "disconnected": id }))
            }
            "restart" => {
                self.restart().await?;
                Ok(json!({ "restarted": true }))
            }
            "snippet" => {
                // The paste-ready DevTools snippet (includes the session token).
                // Exposed for the tray's "Copy console snippet" action; the
                // control socket is owner-only (`0600`), same trust as the token
                // file.
                let ws_port = self.lock().as_ref().map(BridgeServer::ws_port);
                match ws_port {
                    Some(port) => Ok(json!({ "snippet": snippet::render(port, &self.token) })),
                    None => bail!("bridge is not running"),
                }
            }
            other => bail!("unknown browser-bridge op: {other}"),
        }
    }

    fn menu(&self) -> MenuSnapshot {
        let info = self
            .lock()
            .as_ref()
            .map(|s| (s.status(), s.control_port(), s.ws_port()));
        let items = match info {
            Some((status, _control, _ws)) => {
                let line = if status.connected {
                    let origins: Vec<&str> = status
                        .tabs
                        .iter()
                        .filter_map(|t| t.origin.as_deref())
                        .collect();
                    if origins.is_empty() {
                        format!(
                            "Connected — {} tab(s) — {} pending",
                            status.tabs.len(),
                            status.pending
                        )
                    } else {
                        format!(
                            "Connected — {} — {} pending",
                            origins.join(", "),
                            status.pending
                        )
                    }
                } else {
                    "No tab connected".to_string()
                };
                let mut items = vec![MenuItem::Label(line), MenuItem::Separator];
                items.push(MenuItem::Action(MenuAction {
                    id: "copy-snippet".to_string(),
                    label: "Copy console snippet".to_string(),
                    enabled: true,
                }));
                for tab in &status.tabs {
                    items.push(MenuItem::Action(MenuAction {
                        id: format!("disconnect-tab:{}", tab.id),
                        label: format!("Disconnect tab {}", tab.id),
                        enabled: true,
                    }));
                }
                items.push(MenuItem::Action(MenuAction {
                    id: "restart-bridge".to_string(),
                    label: "Restart bridge".to_string(),
                    enabled: true,
                }));
                items
            }
            None => vec![MenuItem::Label("Not running".to_string())],
        };
        MenuSnapshot {
            title: "Browser Bridge".to_string(),
            items,
        }
    }

    async fn menu_action(&self, action_id: &str) -> Result<()> {
        if action_id == "restart-bridge" {
            return self.restart().await;
        }
        if let Some(id_str) = action_id.strip_prefix("disconnect-tab:") {
            let id: u64 = id_str
                .parse()
                .with_context(|| format!("invalid tab id in action {action_id}"))?;
            let guard = self.lock();
            let server = guard
                .as_ref()
                .ok_or_else(|| anyhow!("bridge is not running"))?;
            return server.disconnect_tab(id);
        }
        bail!("unknown browser-bridge menu action: {action_id}")
    }

    async fn status(&self) -> ServiceStatus {
        let info = self
            .lock()
            .as_ref()
            .map(|s| (s.status(), s.control_port(), s.ws_port()));
        match info {
            Some((status, control_port, ws_port)) => ServiceStatus {
                name: SERVICE_NAME.to_string(),
                healthy: true,
                summary: summarize(&status, control_port, ws_port),
                detail: json!({
                    "control_port": control_port,
                    "ws_port": ws_port,
                    "status": status,
                }),
            },
            None => ServiceStatus {
                name: SERVICE_NAME.to_string(),
                healthy: false,
                summary: "not running".to_string(),
                detail: Value::Null,
            },
        }
    }

    async fn shutdown(&self) {
        let server = self.lock().take();
        if let Some(server) = server {
            server.shutdown().await;
        }
        // Best-effort: don't leave a stale token file behind.
        let _ = std::fs::remove_file(&self.token_path);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A bridge service on random ports with its token written to a temp dir.
    async fn temp_service(dir: &Path) -> BridgeService {
        let config = BridgeConfig {
            ws_port: 0,
            control_port: 0,
            ..BridgeConfig::default()
        };
        let token_path = dir.join("bridge.token");
        BridgeService::start(config, None, token_path)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn start_writes_token_and_reports_status() {
        let dir = tempfile::tempdir().unwrap();
        let svc = temp_service(dir.path()).await;

        // Token file exists and is owner-only.
        let token_path = dir.path().join("bridge.token");
        assert!(token_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Status: healthy, no tab connected, ports surfaced in detail.
        let status = svc.status().await;
        assert_eq!(status.name, "browser-bridge");
        assert!(status.healthy);
        assert!(status.detail.get("control_port").is_some());

        // `status` op round-trips a StatusResponse payload.
        let payload = svc.handle("status", Value::Null).await.unwrap();
        assert_eq!(payload.get("connected"), Some(&json!(false)));

        // Unknown op is an error, not a panic.
        assert!(svc.handle("frobnicate", Value::Null).await.is_err());

        svc.shutdown().await;
        // Token file removed on shutdown.
        assert!(!token_path.exists());
    }

    #[tokio::test]
    async fn menu_lists_status_line_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let svc = temp_service(dir.path()).await;
        let menu = svc.menu();
        assert_eq!(menu.title, "Browser Bridge");
        assert!(matches!(menu.items.first(), Some(MenuItem::Label(_))));
        assert!(menu.items.iter().any(|i| matches!(
            i,
            MenuItem::Action(a) if a.id == "restart-bridge"
        )));
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn restart_keeps_service_serving() {
        let dir = tempfile::tempdir().unwrap();
        let svc = temp_service(dir.path()).await;
        svc.handle("restart", Value::Null).await.unwrap();
        // Still healthy after a restart.
        assert!(svc.status().await.healthy);
        svc.shutdown().await;
    }
}
