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
