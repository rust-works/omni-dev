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

#[cfg(all(target_os = "macos", feature = "menu-bar"))]
pub mod tray;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::browser::BridgeConfig;
use registry::ServiceRegistry;
use server::DaemonOptions;
use services::bridge::BridgeService;

/// Everything `daemon run` needs to start the daemon, resolved from the CLI.
///
/// Shared by the headless path ([`run_headless`]) and the macOS menu-bar path
/// (`tray::run`) so both start an identical daemon. The latter is a plain code
/// span, not an intra-doc link, because the `tray` module is feature- and
/// target-gated and absent from the docs build.
#[derive(Debug, Clone)]
pub struct DaemonRunConfig {
    /// Control-socket path (also the single-instance lock).
    pub socket_path: PathBuf,
    /// Browser-bridge configuration (ports, allow-origin, limits).
    pub bridge_config: BridgeConfig,
    /// Optional file the bridge session token is read from instead of generated.
    pub bridge_token_file: Option<PathBuf>,
    /// Where the resolved bridge token is persisted (`0600`) for thin clients.
    pub bridge_token_path: PathBuf,
}

/// Builds the daemon's default service registry: starts the browser bridge on
/// its loopback-TCP planes and registers it.
///
/// `bridge_token_file` overrides token generation; `bridge_token_path` is where
/// the resolved token is persisted (`0600`) for thin-client discovery.
pub async fn build_default_registry(
    bridge_config: BridgeConfig,
    bridge_token_file: Option<&Path>,
    bridge_token_path: PathBuf,
) -> Result<ServiceRegistry> {
    let mut registry = ServiceRegistry::new();
    let bridge = BridgeService::start(bridge_config, bridge_token_file, bridge_token_path).await?;
    registry.register(Arc::new(bridge));
    Ok(registry)
}

/// Runs the daemon headlessly (no tray).
///
/// Builds the registry and serves until a signal or `daemon stop`. The default
/// `daemon run` path on every platform, and the only path when the `menu-bar`
/// feature is off.
pub async fn run_headless(cfg: DaemonRunConfig) -> Result<()> {
    let registry = build_default_registry(
        cfg.bridge_config,
        cfg.bridge_token_file.as_deref(),
        cfg.bridge_token_path,
    )
    .await?;
    server::run(
        registry,
        DaemonOptions {
            socket_path: cfg.socket_path,
        },
    )
    .await
}
