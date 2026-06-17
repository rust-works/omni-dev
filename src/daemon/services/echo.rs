//! A trivial [`DaemonService`] that echoes its payload back. It exists so the
//! daemon framework can be exercised end-to-end before — and independently of
//! — any real service.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::daemon::service::{DaemonService, MenuItem, MenuSnapshot, ServiceStatus};

/// A stateless service whose only op, `echo`, returns its payload verbatim.
#[derive(Debug, Clone, Copy, Default)]
pub struct EchoService;

#[async_trait]
impl DaemonService for EchoService {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn handle(&self, op: &str, payload: Value) -> Result<Value> {
        match op {
            "echo" => Ok(payload),
            other => bail!("unknown echo op: {other}"),
        }
    }

    fn menu(&self) -> MenuSnapshot {
        MenuSnapshot {
            title: "Echo".to_string(),
            items: vec![MenuItem::Label("ready".to_string())],
        }
    }

    async fn menu_action(&self, _action_id: &str) -> Result<()> {
        Ok(())
    }

    async fn status(&self) -> ServiceStatus {
        ServiceStatus {
            name: self.name().to_string(),
            healthy: true,
            summary: "ready".to_string(),
            detail: Value::Null,
        }
    }

    async fn shutdown(&self) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn echo_service_round_trips_and_reports() {
        let svc = EchoService;
        assert_eq!(svc.name(), "echo");
        // `echo` returns its payload verbatim; any other op errors.
        assert_eq!(
            svc.handle("echo", json!({ "a": 1 })).await.unwrap(),
            json!({ "a": 1 })
        );
        assert!(svc.handle("nope", Value::Null).await.is_err());
        // Menu / status / actions are inert but must not panic.
        assert_eq!(svc.menu().title, "Echo");
        svc.menu_action("anything").await.unwrap();
        let status = svc.status().await;
        assert_eq!(status.name, "echo");
        assert!(status.healthy);
        svc.shutdown().await;
    }
}
