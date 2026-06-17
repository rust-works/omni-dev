//! The [`ServiceRegistry`]: the daemon's set of hosted services, plus routing.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::service::{DaemonService, ServiceStatus};

/// Holds the daemon's registered services and routes control-socket envelopes
/// to them by [`name`](DaemonService::name).
#[derive(Clone, Default)]
pub struct ServiceRegistry {
    services: Vec<Arc<dyn DaemonService>>,
}

impl ServiceRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a service. Later lookups match by [`DaemonService::name`]; the
    /// caller is responsible for not registering two services with the same
    /// name (the first registered wins on lookup).
    pub fn register(&mut self, service: Arc<dyn DaemonService>) {
        self.services.push(service);
    }

    /// Returns the registered service with the given name, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn DaemonService>> {
        self.services.iter().find(|s| s.name() == name)
    }

    /// All registered services, in registration order.
    pub fn services(&self) -> &[Arc<dyn DaemonService>] {
        &self.services
    }

    /// Routes an operation to the named service, erroring if no such service is
    /// registered.
    pub async fn dispatch(&self, service: &str, op: &str, payload: Value) -> Result<Value> {
        let svc = self
            .get(service)
            .ok_or_else(|| anyhow!("unknown service: {service}"))?;
        svc.handle(op, payload).await
    }

    /// Collects status from every service, in registration order.
    pub async fn statuses(&self) -> Vec<ServiceStatus> {
        let mut out = Vec::with_capacity(self.services.len());
        for svc in &self.services {
            out.push(svc.status().await);
        }
        out
    }

    /// Gracefully shuts down every service, in registration order.
    pub async fn shutdown_all(&self) {
        for svc in &self.services {
            svc.shutdown().await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::services::echo::EchoService;
    use serde_json::json;

    #[tokio::test]
    async fn routes_known_service_and_rejects_unknown() {
        let mut registry = ServiceRegistry::new();
        assert!(registry.services().is_empty());
        registry.register(Arc::new(EchoService));

        assert!(registry.get("echo").is_some());
        assert!(registry.get("missing").is_none());

        // Routed op reaches the service; an unknown service is an error.
        assert_eq!(
            registry
                .dispatch("echo", "echo", json!({ "x": 1 }))
                .await
                .unwrap(),
            json!({ "x": 1 })
        );
        let err = registry
            .dispatch("missing", "echo", Value::Null)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown service"));

        // Aggregation iterates every registered service.
        assert_eq!(registry.statuses().await.len(), 1);
        registry.shutdown_all().await;
    }
}
