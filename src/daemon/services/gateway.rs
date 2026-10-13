//! The Anthropic gateway daemon service: hosts [`GatewayServer`] under the
//! daemon's lifecycle (#2226).
//!
//! The same engine as the standalone `omni-dev ai gateway serve`; the daemon
//! adds supervision, `daemon status`, a tray submenu and an in-place restart
//! that re-reads the configuration. With no upstream configured the service
//! registers but binds nothing, so a default daemon opens no new port; a
//! configuration error makes the service unhealthy, never the daemon fail.
//! The security model is ADR-0098 and is unchanged by hosting.

use std::path::PathBuf;
use std::sync::{Mutex as StdMutex, MutexGuard};

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::daemon::service::{DaemonService, MenuAction, MenuItem, MenuSnapshot, ServiceStatus};
use crate::gateway::config::{resolve_token, token_path};
use crate::gateway::{ConfigOverrides, GatewayConfig, GatewayConfigError, GatewayServer};

/// The service name (the control-socket routing key).
pub const SERVICE_NAME: &str = "anthropic-gateway";

/// Loads the configuration; injectable for tests.
type Loader = Box<dyn Fn() -> Result<GatewayConfig> + Send + Sync>;

enum Slot {
    /// Serving.
    Running {
        server: GatewayServer,
        summary: String,
        token: String,
    },
    /// No upstream configured; nothing bound.
    NotConfigured,
    /// Configuration or bind failed.
    Failed(String),
    /// Transiently empty during a restart.
    Restarting,
}

/// Hosts the gateway under the daemon.
pub struct GatewayService {
    slot: StdMutex<Slot>,
    /// Serializes restart and shutdown. Without it, two overlapping restarts
    /// could both bind: the second's failure would overwrite the first's
    /// `Running` slot, leaving a server serving with nothing left to stop it.
    lifecycle: tokio::sync::Mutex<()>,
    load: Loader,
    token_path: Option<PathBuf>,
}

impl GatewayService {
    /// Starts the service from the environment and settings.json.
    pub fn start() -> Self {
        let token_path = match token_path() {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!("anthropic gateway token path unavailable: {err:#}");
                None
            }
        };
        Self::start_with(
            Box::new(|| GatewayConfig::from_env(&ConfigOverrides::default())),
            token_path,
        )
    }

    /// Starts the service with an injected loader and token file.
    pub(crate) fn start_with(load: Loader, token_path: Option<PathBuf>) -> Self {
        let service = Self {
            slot: StdMutex::new(Slot::Restarting),
            lifecycle: tokio::sync::Mutex::new(()),
            load,
            token_path,
        };
        let slot = service.launch();
        *service.lock() = slot;
        service
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Loads the configuration and binds the server.
    fn launch(&self) -> Slot {
        let config = match (self.load)() {
            Ok(config) => config,
            Err(err) if err.downcast_ref::<GatewayConfigError>().is_some() => {
                return Slot::NotConfigured
            }
            Err(err) => {
                tracing::warn!("anthropic gateway configuration error: {err:#}");
                return Slot::Failed(format!("{err:#}"));
            }
        };
        let Some(path) = self.token_path.as_deref() else {
            return Slot::Failed("no data directory for the gateway token".to_string());
        };
        let token = match resolve_token(path) {
            Ok(token) => token,
            Err(err) => return Slot::Failed(format!("{err:#}")),
        };
        let summary = config.describe();
        match GatewayServer::start(config, token.clone()) {
            Ok(server) => {
                tracing::info!(
                    port = server.port(),
                    "anthropic gateway listening; {summary}"
                );
                Slot::Running {
                    server,
                    summary,
                    token,
                }
            }
            Err(err) => {
                tracing::warn!("anthropic gateway failed to start: {err:#}");
                Slot::Failed(format!("{err:#}"))
            }
        }
    }

    /// Stops the running server (if any), re-reads the configuration and
    /// starts again. The lock is never held across an await.
    async fn restart(&self) -> Result<()> {
        let _serial = self.lifecycle.lock().await;
        let old = std::mem::replace(&mut *self.lock(), Slot::Restarting);
        if let Slot::Running { server, .. } = old {
            server.shutdown().await;
        }
        let slot = self.launch();
        let failure = match &slot {
            Slot::Failed(message) => Some(message.clone()),
            _ => None,
        };
        *self.lock() = slot;
        match failure {
            Some(message) => bail!("anthropic gateway did not start: {message}"),
            None => Ok(()),
        }
    }

    fn status_value(&self) -> Value {
        match &*self.lock() {
            Slot::Running {
                server, summary, ..
            } => json!({
                "state": "running",
                "port": server.port(),
                "base_url": GatewayConfig::base_url_for(server.port()),
                "upstream": summary,
                "stats": server.stats(),
            }),
            Slot::NotConfigured => json!({"state": "not-configured"}),
            Slot::Failed(message) => json!({"state": "failed", "error": message}),
            Slot::Restarting => json!({"state": "restarting"}),
        }
    }

    fn summary(&self) -> (bool, String) {
        match &*self.lock() {
            Slot::Running { server, .. } => {
                let stats = server.stats();
                (
                    true,
                    format!(
                        "listening on 127.0.0.1:{} — {} request(s), {} error(s), {} in flight",
                        server.port(),
                        stats.requests,
                        stats.errors,
                        stats.in_flight
                    ),
                )
            }
            Slot::NotConfigured => (
                true,
                "not configured (set OMNI_DEV_GATEWAY_UPSTREAM_URL, then restart)".to_string(),
            ),
            Slot::Failed(message) => (false, format!("failed: {message}")),
            Slot::Restarting => (true, "restarting".to_string()),
        }
    }
}

#[async_trait]
impl DaemonService for GatewayService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    async fn handle(&self, op: &str, _payload: Value) -> Result<Value> {
        match op {
            "status" => Ok(self.status_value()),
            "restart" => {
                self.restart().await?;
                Ok(self.status_value())
            }
            "env" => {
                // The client settings, token included; the control socket is
                // owner-only (`0600`), the same trust as the token file.
                match &*self.lock() {
                    Slot::Running { server, token, .. } => Ok(json!({
                        "ANTHROPIC_BASE_URL": GatewayConfig::base_url_for(server.port()),
                        "ANTHROPIC_AUTH_TOKEN": token,
                    })),
                    _ => bail!("the anthropic gateway is not running"),
                }
            }
            other => bail!("unknown {SERVICE_NAME} op: {other}"),
        }
    }

    fn menu(&self) -> MenuSnapshot {
        let (_, line) = self.summary();
        let mut items = vec![MenuItem::Label(line)];
        if let Slot::Running { summary, .. } = &*self.lock() {
            items.push(MenuItem::Label(summary.clone()));
        }
        items.push(MenuItem::Separator);
        items.push(MenuItem::Action(MenuAction {
            id: "restart".to_string(),
            label: "Restart gateway (re-read configuration)".to_string(),
            enabled: true,
        }));
        MenuSnapshot {
            title: "Anthropic Gateway".to_string(),
            items,
        }
    }

    async fn menu_action(&self, action_id: &str) -> Result<()> {
        match action_id {
            "restart" => self.restart().await,
            other => bail!("unknown {SERVICE_NAME} menu action: {other}"),
        }
    }

    async fn status(&self) -> ServiceStatus {
        let (healthy, summary) = self.summary();
        ServiceStatus {
            name: SERVICE_NAME.to_string(),
            healthy,
            summary,
            detail: self.status_value(),
        }
    }

    async fn shutdown(&self) {
        let _serial = self.lifecycle.lock().await;
        let old = std::mem::replace(&mut *self.lock(), Slot::Restarting);
        if let Slot::Running { server, .. } = old {
            server.shutdown().await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    fn loader(vars: Vec<(&'static str, String)>) -> Loader {
        Box::new(move || {
            let mut env = MapEnv::new();
            for (k, v) in &vars {
                env = env.with(k, v);
            }
            GatewayConfig::from_env_with(&env, &ConfigOverrides::default())
        })
    }

    #[tokio::test]
    async fn an_unconfigured_service_is_healthy_and_binds_nothing() {
        let service = GatewayService::start_with(loader(vec![]), None);
        assert_eq!(service.name(), SERVICE_NAME);
        let status = service.status().await;
        assert!(status.healthy);
        assert!(status.summary.contains("not configured"));
        assert_eq!(
            service.handle("status", Value::Null).await.unwrap()["state"],
            "not-configured"
        );
        assert!(service.handle("env", Value::Null).await.is_err());
        assert!(service.handle("bogus", Value::Null).await.is_err());
        assert!(service.menu_action("bogus").await.is_err());
        assert_eq!(service.menu().title, "Anthropic Gateway");
        service.shutdown().await;
    }

    #[tokio::test]
    async fn a_bad_configuration_is_unhealthy() {
        let vars = vec![
            (
                "OMNI_DEV_GATEWAY_UPSTREAM_URL",
                "http://127.0.0.1:1".to_string(),
            ),
            ("OMNI_DEV_GATEWAY_FLAVOR", "nonsense".to_string()),
        ];
        let service = GatewayService::start_with(loader(vars), None);
        let status = service.status().await;
        assert!(!status.healthy);
        assert!(status.summary.contains("nonsense"));
        assert!(service.handle("restart", Value::Null).await.is_err());

        let missing_token_dir = vec![(
            "OMNI_DEV_GATEWAY_UPSTREAM_URL",
            "http://127.0.0.1:1".to_string(),
        )];
        let service = GatewayService::start_with(loader(missing_token_dir), None);
        assert!(service.status().await.summary.contains("data directory"));
    }

    #[tokio::test]
    async fn a_configured_service_serves_reports_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("gateway.token");
        let vars = vec![
            (
                "OMNI_DEV_GATEWAY_UPSTREAM_URL",
                "http://127.0.0.1:1".to_string(),
            ),
            ("OMNI_DEV_GATEWAY_PORT", "0".to_string()),
            ("OMNI_DEV_GATEWAY_MODEL", "m".to_string()),
        ];
        let service = GatewayService::start_with(loader(vars), Some(token_file.clone()));
        let status = service.handle("status", Value::Null).await.unwrap();
        assert_eq!(status["state"], "running");
        let port = status["port"].as_u64().unwrap();
        assert!(status["upstream"].as_str().unwrap().contains("127.0.0.1:1"));

        let env = service.handle("env", Value::Null).await.unwrap();
        let token = std::fs::read_to_string(&token_file).unwrap();
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], token.trim());
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://127.0.0.1:{port}/v1/models"))
            .header("x-api-key", token.trim())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let healthy = service.status().await;
        assert!(healthy.healthy);
        assert!(healthy.summary.contains("listening"));
        assert_eq!(service.menu().items.len(), 4);
        service.menu_action("restart").await.unwrap();
        assert_eq!(
            service.handle("status", Value::Null).await.unwrap()["state"],
            "running"
        );
        service.shutdown().await;
        assert_eq!(
            service.handle("status", Value::Null).await.unwrap()["state"],
            "restarting"
        );
    }

    #[tokio::test]
    async fn overlapping_restarts_leave_one_tracked_server() {
        let dir = tempfile::tempdir().unwrap();
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = squatter.local_addr().unwrap().port();
        drop(squatter);
        let vars = vec![
            (
                "OMNI_DEV_GATEWAY_UPSTREAM_URL",
                "http://127.0.0.1:1".to_string(),
            ),
            ("OMNI_DEV_GATEWAY_PORT", port.to_string()),
        ];
        let service = GatewayService::start_with(loader(vars), Some(dir.path().join("t")));
        let (a, b) = tokio::join!(service.restart(), service.restart());
        a.unwrap();
        b.unwrap();
        let status = service.handle("status", Value::Null).await.unwrap();
        assert_eq!(status["state"], "running");
        assert_eq!(status["port"], port);
        // The tracked server is the one holding the port: shutting it down frees it.
        service.shutdown().await;
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[tokio::test]
    async fn a_taken_port_fails_without_killing_the_daemon() {
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let vars = vec![
            (
                "OMNI_DEV_GATEWAY_UPSTREAM_URL",
                "http://127.0.0.1:1".to_string(),
            ),
            ("OMNI_DEV_GATEWAY_PORT", port.to_string()),
        ];
        let service = GatewayService::start_with(loader(vars), Some(dir.path().join("t")));
        let status = service.status().await;
        assert!(!status.healthy);
        assert!(status.summary.contains("already in use"));
    }
}
