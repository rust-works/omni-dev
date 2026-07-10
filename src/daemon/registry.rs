//! The [`ServiceRegistry`]: the daemon's set of hosted services, plus routing.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::service::{DaemonService, ServiceStatus, ServiceStream};

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

    /// Adds a service. Lookups match by [`DaemonService::name`] and the first
    /// registration wins; a second service sharing a name would be dead code
    /// for routing and would double-count in status/menu iteration, so it is
    /// rejected with a warning rather than silently kept.
    pub fn register(&mut self, service: Arc<dyn DaemonService>) {
        let name = service.name();
        if self.services.iter().any(|s| s.name() == name) {
            tracing::warn!("ignoring duplicate registration of daemon service `{name}`");
            return;
        }
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

    /// Opens a push subscription on the named service for a streaming `op`, or
    /// `None` when the service is unknown or does not stream that op — in which
    /// case the caller falls back to the normal [`dispatch`](Self::dispatch)
    /// request→reply path (#1267).
    pub fn subscribe(
        &self,
        service: &str,
        op: &str,
        payload: &Value,
    ) -> Option<Box<dyn ServiceStream>> {
        self.get(service)?.subscribe(op, payload)
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

    #[test]
    fn duplicate_registration_is_ignored() {
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(EchoService));
        registry.register(Arc::new(EchoService));
        // The second registration shares `echo`'s name, so it is dropped: the
        // first wins on lookup and iteration is not double-counted.
        assert_eq!(registry.services().len(), 1);
    }
}
