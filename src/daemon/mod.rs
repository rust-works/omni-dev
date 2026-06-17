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
//!   macOS); `stop` / `restart` / `status` are thin [`client::DaemonClient`]s.

pub mod client;
pub mod lifecycle;
pub mod paths;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod service;
pub mod services;
pub mod single_instance;

#[cfg(target_os = "macos")]
pub mod launchd;

use std::sync::Arc;

use anyhow::Result;

use registry::ServiceRegistry;
use services::echo::EchoService;

/// Builds the daemon's default service registry.
///
/// Registers only the [`EchoService`] for now; the browser bridge service joins
/// it in a later change (#987). Kept `async` because real services (the bridge)
/// perform asynchronous startup when constructed here.
pub async fn build_default_registry() -> Result<ServiceRegistry> {
    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(EchoService));
    Ok(registry)
}
