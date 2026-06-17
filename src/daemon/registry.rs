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
