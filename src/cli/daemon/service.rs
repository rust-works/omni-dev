//! `omni-dev daemon service` — a low-level passthrough to any daemon service op.
//!
//! Sends one `<SERVICE> <OP>` envelope over the control socket and prints the
//! raw reply payload. A generic escape hatch complementing the typed per-service
//! commands (`daemon bridge`, `snowflake`, `worktrees`): it reaches **any** op on
//! **any** registered service, so a new service needs no bespoke CLI to be
//! operable. `daemon status` lists the service names and each service's ops are
//! documented in its operator guide.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::Value;

use crate::daemon::server;

/// Sends an arbitrary operation to a named daemon service.
#[derive(Parser)]
pub struct ServiceCommand {
    /// Target service name (e.g. `browser-bridge`, `snowflake`, `worktrees`).
    #[arg(value_name = "SERVICE")]
    pub service: String,
    /// Operation name, interpreted by that service (e.g. `status`, `restart`).
    #[arg(value_name = "OP")]
    pub op: String,
    /// JSON payload for the op (default: none).
    #[arg(long, value_name = "JSON")]
    pub payload: Option<String>,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl ServiceCommand {
    /// Executes the service passthrough command.
    pub async fn execute(self) -> Result<()> {
        let payload = parse_payload(self.payload.as_deref())?;
        let socket = server::resolve_socket(self.socket)?;
        let reply = super::call_service(&socket, &self.service, &self.op, payload).await?;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        Ok(())
    }
}

/// Parses the optional `--payload` JSON, defaulting to `null` (no arguments).
fn parse_payload(raw: Option<&str>) -> Result<Value> {
    match raw {
        Some(raw) => serde_json::from_str(raw)
            .with_context(|| format!("`--payload` is not valid JSON: {raw}")),
        None => Ok(Value::Null),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_payload_defaults_to_null_and_parses_json() {
        assert_eq!(parse_payload(None).unwrap(), Value::Null);
        assert_eq!(
            parse_payload(Some(r#"{"id":3}"#)).unwrap(),
            serde_json::json!({ "id": 3 })
        );
        assert!(parse_payload(Some("{not json")).is_err());
    }

    #[test]
    fn parses_service_op_and_flags() {
        let cmd = ServiceCommand::try_parse_from([
            "service",
            "browser-bridge",
            "disconnect-tab",
            "--payload",
            r#"{"id":3}"#,
            "--socket",
            "/tmp/d.sock",
        ])
        .unwrap();
        assert_eq!(cmd.service, "browser-bridge");
        assert_eq!(cmd.op, "disconnect-tab");
        assert_eq!(cmd.payload.as_deref(), Some(r#"{"id":3}"#));
        assert_eq!(
            cmd.socket.as_deref(),
            Some(std::path::Path::new("/tmp/d.sock"))
        );

        // SERVICE and OP are both required positionals.
        assert!(ServiceCommand::try_parse_from(["service", "browser-bridge"]).is_err());
    }

    #[tokio::test]
    async fn execute_sends_the_op_and_prints_the_reply() {
        let (_dir, sock, server) = crate::daemon::testutil::fake_daemon_reply(
            serde_json::json!({ "ok": true, "payload": { "connected": false } }),
        );
        let cmd = ServiceCommand {
            service: "browser-bridge".to_string(),
            op: "status".to_string(),
            payload: None,
            socket: Some(sock),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn execute_rejects_malformed_payload_before_touching_the_socket() {
        // A bad `--payload` fails at parse time, so no daemon is needed.
        let cmd = ServiceCommand {
            service: "browser-bridge".to_string(),
            op: "disconnect-tab".to_string(),
            payload: Some("{not json".to_string()),
            socket: Some(PathBuf::from("/nonexistent/omni-dev-svc.sock")),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }
}
