//! The [`DaemonService`] abstraction: a pluggable unit of work hosted by the
//! daemon supervisor.
//!
//! A service has a stable [`name`](DaemonService::name), answers operations
//! routed to it over the control socket ([`handle`](DaemonService::handle)),
//! contributes a tray submenu ([`menu`](DaemonService::menu) /
//! [`menu_action`](DaemonService::menu_action)), reports structured status for
//! `daemon status` ([`status`](DaemonService::status)), and participates in
//! graceful shutdown ([`shutdown`](DaemonService::shutdown)). See ADR-0039.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A long-lived unit of work supervised by the daemon.
///
/// Implementations are registered in a [`ServiceRegistry`](super::registry::ServiceRegistry)
/// and are assumed live from registration until [`shutdown`](Self::shutdown).
#[async_trait]
pub trait DaemonService: Send + Sync {
    /// Stable identifier used to route control-socket envelopes to this service
    /// (the envelope's `service` field) and to label its status/menu.
    fn name(&self) -> &'static str;

    /// Handles an operation routed to this service, returning a JSON payload.
    async fn handle(&self, op: &str, payload: Value) -> Result<Value>;

    /// Cheap snapshot of the service's tray submenu, polled by the menu-bar
    /// shell. Must not block.
    fn menu(&self) -> MenuSnapshot;

    /// Performs a tray menu action previously surfaced by [`menu`](Self::menu),
    /// identified by its [`MenuAction::id`].
    async fn menu_action(&self, action_id: &str) -> Result<()>;

    /// Structured status for `daemon status` aggregation.
    async fn status(&self) -> ServiceStatus;

    /// Gracefully stops the service, draining in-flight work. Called once on
    /// daemon shutdown.
    async fn shutdown(&self);
}

/// Structured per-service status, aggregated by the built-in `status` op and
/// surfaced by `omni-dev daemon status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// The reporting service's [`name`](DaemonService::name).
    pub name: String,
    /// Whether the service is currently operating normally.
    pub healthy: bool,
    /// One-line human-readable summary (e.g. `"1 tab connected"`).
    pub summary: String,
    /// Service-specific structured detail; `null` when there is none.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub detail: Value,
}

/// A cheap snapshot of a service's tray submenu, rebuilt on each poll.
#[derive(Debug, Clone, Default)]
pub struct MenuSnapshot {
    /// Submenu title (the parent menu-bar entry label).
    pub title: String,
    /// Ordered submenu entries.
    pub items: Vec<MenuItem>,
}

/// A single entry in a [`MenuSnapshot`].
#[derive(Debug, Clone)]
pub enum MenuItem {
    /// A non-interactive status line.
    Label(String),
    /// A horizontal separator.
    Separator,
    /// A clickable action dispatched via [`DaemonService::menu_action`].
    Action(MenuAction),
}

/// A clickable tray action.
#[derive(Debug, Clone)]
pub struct MenuAction {
    /// Stable identifier passed back to [`DaemonService::menu_action`].
    pub id: String,
    /// Human-readable menu label.
    pub label: String,
    /// Whether the action is currently selectable.
    pub enabled: bool,
}
