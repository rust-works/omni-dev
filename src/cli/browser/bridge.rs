//! `omni-dev browser bridge` — server (`serve`) and thin client (`request`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use super::harvest::HarvestCommand;
use super::request::RequestCommand;
use crate::browser::bridge::{
    DEFAULT_CONTROL_PORT, DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_CONCURRENT, DEFAULT_TIMEOUT_SECS,
    DEFAULT_WS_PORT,
};
use crate::browser::{self, auth, BridgeConfig};

/// Bridge: run the server (`serve`) or send a request through it (`request`).
#[derive(Parser)]
pub struct BridgeCommand {
    /// The bridge subcommand to execute.
    #[command(subcommand)]
    pub command: BridgeSubcommands,
}

/// Bridge subcommands.
#[derive(Subcommand)]
pub enum BridgeSubcommands {
    /// Runs the local bridge server (WebSocket + HTTP control planes).
    Serve(ServeCommand),
    /// Sends a request through a running bridge (thin client).
    Request(RequestCommand),
    /// Harvests your own data from a logged-in tab (best-effort).
    Harvest(HarvestCommand),
}

impl BridgeCommand {
    /// Executes the bridge command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            BridgeSubcommands::Serve(cmd) => cmd.execute().await,
            BridgeSubcommands::Request(cmd) => cmd.execute().await,
            BridgeSubcommands::Harvest(cmd) => cmd.execute().await,
        }
    }
}

/// Runs the local bridge server (WebSocket + HTTP control planes).
///
/// Generates a session token at startup (unless `--token-file` or
/// `OMNI_BRIDGE_TOKEN` supplies one) and prints it with a ready-to-paste
/// browser snippet. Both planes bind loopback only and fail closed if a
/// requested port is already in use. Pass `0` for `--ws-port` / `--control-port`
/// to bind an OS-assigned random port.
#[derive(Parser)]
pub struct ServeCommand {
    /// WebSocket-plane port. `0` binds a random free port.
    #[arg(long, default_value_t = DEFAULT_WS_PORT)]
    pub ws_port: u16,

    /// HTTP control-plane port. `0` binds a random free port.
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    pub control_port: u16,

    /// Per-request timeout in seconds before the control plane returns `504`.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub request_timeout: u64,

    /// Permit this exact cross-origin for both the WebSocket upgrade and
    /// outbound request URLs (e.g. `https://grafana.internal`). Without it,
    /// only same-origin (relative) URLs are allowed.
    #[arg(long, value_name = "URL")]
    pub allow_origin: Option<String>,

    /// Maximum browser response body size accepted, in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_BODY_BYTES)]
    pub max_body_bytes: usize,

    /// Maximum number of concurrent in-flight requests.
    #[arg(long, default_value_t = DEFAULT_MAX_CONCURRENT)]
    pub max_concurrent: usize,

    /// Read the session token from this `0600` file instead of generating one.
    /// The token is never accepted as a command-line argument.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
}

impl ServeCommand {
    /// Executes the serve command.
    pub async fn execute(self) -> Result<()> {
        let token = auth::resolve_token(self.token_file.as_deref())?;
        let config = BridgeConfig {
            ws_port: self.ws_port,
            control_port: self.control_port,
            request_timeout: Duration::from_secs(self.request_timeout),
            allow_origin: self.allow_origin,
            max_body_bytes: self.max_body_bytes,
            max_concurrent: self.max_concurrent.max(1),
        };
        browser::run(config, token).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Mirrors the `omni-dev browser bridge serve` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: Cmd,
    }
    #[derive(clap::Subcommand)]
    enum Cmd {
        Serve(ServeCommand),
    }

    fn parse(args: &[&str]) -> ServeCommand {
        let mut full = vec!["omni-dev", "serve"];
        full.extend_from_slice(args);
        let Wrapper { cmd: Cmd::Serve(c) } = Wrapper::try_parse_from(full).unwrap();
        c
    }

    #[test]
    fn defaults_match_documented_ports() {
        let c = parse(&[]);
        assert_eq!(c.ws_port, 9999);
        assert_eq!(c.control_port, 9998);
        assert_eq!(c.request_timeout, 30);
        assert!(c.allow_origin.is_none());
        assert!(c.token_file.is_none());
    }

    #[test]
    fn random_port_zero_is_accepted() {
        let c = parse(&["--ws-port", "0", "--control-port", "0"]);
        assert_eq!(c.ws_port, 0);
        assert_eq!(c.control_port, 0);
    }

    #[test]
    fn flags_are_parsed() {
        let c = parse(&[
            "--request-timeout",
            "5",
            "--allow-origin",
            "https://ok.test",
            "--max-body-bytes",
            "1024",
            "--max-concurrent",
            "8",
        ]);
        assert_eq!(c.request_timeout, 5);
        assert_eq!(c.allow_origin.as_deref(), Some("https://ok.test"));
        assert_eq!(c.max_body_bytes, 1024);
        assert_eq!(c.max_concurrent, 8);
    }

    #[test]
    fn token_is_not_a_flag() {
        // The session token must never be settable via argv.
        let mut full = vec!["omni-dev", "serve", "--token", "secret"];
        assert!(Wrapper::try_parse_from(std::mem::take(&mut full)).is_err());
    }

    /// Builds a `ServeCommand` whose control plane targets `control_port`.
    fn serve_cmd(control_port: u16) -> ServeCommand {
        ServeCommand {
            ws_port: 0,
            control_port,
            request_timeout: DEFAULT_TIMEOUT_SECS,
            allow_origin: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            token_file: None,
        }
    }

    /// The `serve` arm reaches the server, which fails closed when its control
    /// port is already taken — exercising dispatch without a long-lived bind.
    #[tokio::test]
    async fn dispatch_serve_arm_surfaces_bind_failure() {
        // Occupy a port, then ask serve to bind the same one.
        let squatter = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let taken = squatter.local_addr().unwrap().port();
        let cmd = BridgeCommand {
            command: BridgeSubcommands::Serve(serve_cmd(taken)),
        };
        assert!(cmd.execute().await.is_err());
    }

    /// The `request` arm reaches the thin client, which errors with no bridge
    /// listening — exercising dispatch into the request path.
    #[tokio::test]
    async fn dispatch_request_arm_reaches_client() {
        let cmd = BridgeCommand {
            command: BridgeSubcommands::Request(RequestCommand {
                url: "/x".to_string(),
                method: "GET".to_string(),
                headers: Vec::new(),
                body: None,
                body_file: None,
                // Port 0 never has a listener, so the client fails fast.
                control_port: 0,
                token_file: None,
                stream: false,
                target: None,
                allow_origin: None,
                credentials: None,
            }),
        };
        assert!(cmd.execute().await.is_err());
    }
}
