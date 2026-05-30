//! `omni-dev browser bridge` — runs the long-lived bridge server.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use crate::browser::{self, auth, BridgeConfig};

/// Default WebSocket-plane port.
const DEFAULT_WS_PORT: u16 = 9999;
/// Default HTTP control-plane port.
const DEFAULT_CONTROL_PORT: u16 = 9998;
/// Default per-request timeout, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default maximum browser response body size (8 MiB).
const DEFAULT_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum concurrent in-flight requests.
const DEFAULT_MAX_CONCURRENT: usize = 64;

/// Runs the local bridge server (WebSocket + HTTP control planes).
///
/// Generates a session token at startup (unless `--token-file` or
/// `OMNI_BRIDGE_TOKEN` supplies one) and prints it with a ready-to-paste
/// browser snippet. Both planes bind loopback only and fail closed if a
/// requested port is already in use. Pass `0` for `--ws-port` / `--control-port`
/// to bind an OS-assigned random port.
#[derive(Parser)]
pub struct BridgeCommand {
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

impl BridgeCommand {
    /// Executes the bridge command.
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

    /// Mirrors the `omni-dev browser bridge` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: Cmd,
    }
    #[derive(clap::Subcommand)]
    enum Cmd {
        Bridge(BridgeCommand),
    }

    fn parse(args: &[&str]) -> BridgeCommand {
        let mut full = vec!["omni-dev", "bridge"];
        full.extend_from_slice(args);
        let Wrapper {
            cmd: Cmd::Bridge(c),
        } = Wrapper::try_parse_from(full).unwrap();
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
        let mut full = vec!["omni-dev", "bridge", "--token", "secret"];
        assert!(Wrapper::try_parse_from(std::mem::take(&mut full)).is_err());
    }
}
