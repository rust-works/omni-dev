//! `omni-dev daemon run` — become the daemon (foreground).

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::browser::bridge::{DEFAULT_CONTROL_PORT, DEFAULT_WS_PORT};
use crate::browser::BridgeConfig;
use crate::daemon::server;
use crate::daemon::{self, paths, DaemonRunConfig};

/// Runs the daemon in the foreground.
///
/// Acquires the control socket — the launchd-activated fd when the daemon is
/// socket-activated, otherwise a self-bound socket (which doubles as the
/// single-instance lock) — starts every registered service (including the browser
/// bridge on its loopback-TCP planes), and blocks until `SIGTERM`/`SIGINT` or a
/// `daemon stop`. This is the process a launchd LaunchAgent demand-spawns on the
/// first client connect (or that `daemon start` warms).
///
/// On a macOS build with the `menu-bar` feature, this also shows the menu-bar
/// tray unless `--no-menu` is given.
#[derive(Parser, Clone)]
pub struct RunCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Browser-bridge WebSocket-plane port (`0` binds a random free port).
    #[arg(long, default_value_t = DEFAULT_WS_PORT)]
    pub bridge_ws_port: u16,

    /// Browser-bridge HTTP control-plane port (`0` binds a random free port).
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    pub bridge_control_port: u16,

    /// Permit a cross-origin for the bridge's WebSocket upgrade and outbound
    /// URLs (the bridge's `--allow-origin`). Repeatable and scoped per connecting
    /// tab: a bare `ORIGIN`, or a `CONNECT=OUTBOUND` mapping.
    #[arg(long, value_name = "URL[=URL]")]
    pub bridge_allow_origin: Vec<String>,

    /// Read the bridge session token from this `0600` file instead of
    /// generating one.
    #[arg(long, value_name = "PATH")]
    pub bridge_token_file: Option<PathBuf>,

    /// Run headless: never show the macOS menu-bar tray (no effect on non-macOS
    /// or non-`menu-bar` builds, which are always headless).
    #[arg(long)]
    pub no_menu: bool,
}

impl RunCommand {
    /// Executes the run command (headless; the menu-bar path is selected in
    /// `main`, which hands the main thread to the tray).
    pub async fn execute(self) -> Result<()> {
        daemon::run_headless(self.into_run_config()?).await
    }

    /// Resolves the CLI flags into a [`DaemonRunConfig`]. Shared by the headless
    /// `execute` path and the macOS menu-bar path.
    pub fn into_run_config(self) -> Result<DaemonRunConfig> {
        let socket_path = server::resolve_socket(self.socket)?;
        let bridge_token_path = paths::token_path_for_socket(&socket_path);
        let allow_origins = crate::browser::auth::OriginAllowlist::parse(&self.bridge_allow_origin)
            .map_err(|e| anyhow::anyhow!("invalid --bridge-allow-origin: {e}"))?;
        let bridge_config = BridgeConfig {
            ws_port: self.bridge_ws_port,
            control_port: self.bridge_control_port,
            allow_origins,
            ..BridgeConfig::default()
        };
        Ok(DaemonRunConfig {
            socket_path,
            bridge_config,
            bridge_token_file: self.bridge_token_file,
            bridge_token_path,
        })
    }
}
