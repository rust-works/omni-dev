//! `omni-dev daemon bridge` — control the daemon-hosted browser bridge.
//!
//! Typed subcommands over the daemon control socket for the bridge ops that were
//! previously reachable only from the macOS tray (#1113): restart the server,
//! disconnect a tab, read status, or copy the DevTools snippet / session token /
//! ready-to-run request command. These drive the daemon's `browser-bridge`
//! service; they are distinct from `omni-dev browser bridge request`, which talks
//! to the bridge's own loopback-TCP control plane (ADR-0036).

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::cli::format::TableOrJson;
use crate::daemon::server;

/// The browser-bridge service routing key on the daemon control socket.
const SERVICE: &str = "browser-bridge";

/// Bridge: control the daemon-hosted browser bridge from the CLI (no tray needed).
#[derive(Parser)]
pub struct BridgeCommand {
    /// The bridge subcommand to execute.
    #[command(subcommand)]
    pub command: BridgeSubcommands,
}

/// Bridge subcommands.
#[derive(Subcommand)]
pub enum BridgeSubcommands {
    /// Show the bridge's status (connected tabs, pending requests, ports).
    Status(StatusArgs),
    /// Restart the bridge server (tears down and rebinds its loopback planes).
    Restart(SocketArg),
    /// Disconnect one connected browser tab by its numeric id.
    DisconnectTab(DisconnectTabArgs),
    /// Print the paste-ready DevTools console snippet (embeds the session token).
    Snippet(SocketArg),
    /// Print the raw bridge session token (the "bridge key").
    Token(SocketArg),
    /// Print a ready-to-run `browser bridge request` command with the token set.
    RequestCommand(SocketArg),
}

impl BridgeCommand {
    /// Executes the bridge command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            BridgeSubcommands::Status(args) => args.execute().await,
            BridgeSubcommands::Restart(args) => {
                let socket = server::resolve_socket(args.socket)?;
                super::call_service(&socket, SERVICE, "restart", Value::Null).await?;
                println!("bridge restarted");
                Ok(())
            }
            BridgeSubcommands::DisconnectTab(args) => {
                let socket = server::resolve_socket(args.socket)?;
                super::call_service(&socket, SERVICE, "disconnect-tab", json!({ "id": args.id }))
                    .await?;
                println!("disconnected tab {}", args.id);
                Ok(())
            }
            BridgeSubcommands::Snippet(args) => {
                print_field(args.socket, "snippet", "snippet").await
            }
            BridgeSubcommands::Token(args) => print_field(args.socket, "token", "token").await,
            BridgeSubcommands::RequestCommand(args) => {
                print_field(args.socket, "request-command", "command").await
            }
        }
    }
}

/// The bare `--socket` argument shared by the ops that take no other input.
#[derive(Parser)]
pub struct SocketArg {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// `disconnect-tab` arguments: the numeric tab id plus the socket override.
#[derive(Parser)]
pub struct DisconnectTabArgs {
    /// The numeric tab id to disconnect (see `daemon bridge status`).
    #[arg(value_name = "ID")]
    pub id: u64,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// `status` arguments: the socket override plus the output format.
#[derive(Parser)]
pub struct StatusArgs {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
}

impl StatusArgs {
    /// Executes the bridge status command.
    async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let status = super::call_service(&socket, SERVICE, "status", Value::Null).await?;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&status)?),
            TableOrJson::Table => println!("{}", render_status(&status)),
        }
        Ok(())
    }
}

/// Calls a bridge op whose reply is a single string field and prints that field
/// (e.g. `snippet`/`snippet`, `token`/`token`, `request-command`/`command`).
async fn print_field(socket: Option<PathBuf>, op: &str, field: &str) -> Result<()> {
    let socket = server::resolve_socket(socket)?;
    let payload = super::call_service(&socket, SERVICE, op, Value::Null).await?;
    match payload.get(field).and_then(Value::as_str) {
        Some(value) => {
            println!("{value}");
            Ok(())
        }
        None => bail!("bridge `{op}` returned no `{field}` field"),
    }
}

/// Renders a bridge `status` reply as a one-line human summary. The reply is the
/// serialized `StatusResponse` when the bridge is running, or `{ "running": false }`
/// when its server slot is empty. `-o json` prints the full payload instead.
fn render_status(status: &Value) -> String {
    if status.get("running").and_then(Value::as_bool) == Some(false) {
        return "bridge: not running".to_string();
    }
    let tabs = status
        .get("tabs")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let pending = status.get("pending").and_then(Value::as_u64).unwrap_or(0);
    if status.get("connected").and_then(Value::as_bool) == Some(true) {
        format!("bridge: {tabs} tab(s) connected, {pending} pending")
    } else {
        "bridge: no tab connected".to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Mirrors the `omni-dev daemon bridge` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: BridgeSubcommands,
    }

    fn parse(args: &[&str]) -> BridgeSubcommands {
        let mut full = vec!["omni-dev"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full).unwrap().cmd
    }

    #[test]
    fn parses_every_subcommand() {
        assert!(matches!(parse(&["status"]), BridgeSubcommands::Status(_)));
        assert!(matches!(parse(&["restart"]), BridgeSubcommands::Restart(_)));
        assert!(matches!(
            parse(&["disconnect-tab", "3"]),
            BridgeSubcommands::DisconnectTab(_)
        ));
        assert!(matches!(parse(&["snippet"]), BridgeSubcommands::Snippet(_)));
        assert!(matches!(parse(&["token"]), BridgeSubcommands::Token(_)));
        assert!(matches!(
            parse(&["request-command"]),
            BridgeSubcommands::RequestCommand(_)
        ));
    }

    #[test]
    fn disconnect_tab_requires_a_numeric_id() {
        let BridgeSubcommands::DisconnectTab(args) = parse(&["disconnect-tab", "7"]) else {
            panic!("expected disconnect-tab");
        };
        assert_eq!(args.id, 7);
        // A missing or non-numeric id is a parse error.
        assert!(DisconnectTabArgs::try_parse_from(["disconnect-tab"]).is_err());
        assert!(DisconnectTabArgs::try_parse_from(["disconnect-tab", "abc"]).is_err());
    }

    #[test]
    fn render_status_summarizes_the_reply() {
        assert_eq!(
            render_status(&json!({ "running": false })),
            "bridge: not running"
        );
        assert_eq!(
            render_status(&json!({ "connected": false, "tabs": [], "pending": 0 })),
            "bridge: no tab connected"
        );
        assert_eq!(
            render_status(&json!({
                "connected": true,
                "tabs": [ { "id": 1 }, { "id": 2 } ],
                "pending": 3
            })),
            "bridge: 2 tab(s) connected, 3 pending"
        );
    }

    use crate::daemon::testutil::fake_daemon_reply;

    #[tokio::test]
    async fn restart_dispatches_the_restart_op() {
        let (_dir, sock, server) =
            fake_daemon_reply(json!({ "ok": true, "payload": { "restarted": true } }));
        let cmd = BridgeCommand {
            command: BridgeSubcommands::Restart(SocketArg { socket: Some(sock) }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_tab_dispatches_with_the_id() {
        let (_dir, sock, server) =
            fake_daemon_reply(json!({ "ok": true, "payload": { "disconnected": 3 } }));
        let cmd = BridgeCommand {
            command: BridgeSubcommands::DisconnectTab(DisconnectTabArgs {
                id: 3,
                socket: Some(sock),
            }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn status_renders_an_ok_reply() {
        let (_dir, sock, server) = fake_daemon_reply(
            json!({ "ok": true, "payload": { "connected": false, "tabs": [], "pending": 0 } }),
        );
        let cmd = BridgeCommand {
            command: BridgeSubcommands::Status(StatusArgs {
                socket: Some(sock),
                output: TableOrJson::Table,
            }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn token_prints_the_token_field() {
        let (_dir, sock, server) =
            fake_daemon_reply(json!({ "ok": true, "payload": { "token": "abc123" } }));
        let cmd = BridgeCommand {
            command: BridgeSubcommands::Token(SocketArg { socket: Some(sock) }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn snippet_prints_the_snippet_field() {
        let (_dir, sock, server) =
            fake_daemon_reply(json!({ "ok": true, "payload": { "snippet": "paste-me" } }));
        let cmd = BridgeCommand {
            command: BridgeSubcommands::Snippet(SocketArg { socket: Some(sock) }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn request_command_prints_the_command_field() {
        let (_dir, sock, server) = fake_daemon_reply(
            json!({ "ok": true, "payload": { "command": "OMNI_BRIDGE_TOKEN=… …" } }),
        );
        let cmd = BridgeCommand {
            command: BridgeSubcommands::RequestCommand(SocketArg { socket: Some(sock) }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn status_renders_json_output() {
        let (_dir, sock, server) = fake_daemon_reply(
            json!({ "ok": true, "payload": { "connected": true, "tabs": [{ "id": 1 }], "pending": 0 } }),
        );
        let cmd = BridgeCommand {
            command: BridgeSubcommands::Status(StatusArgs {
                socket: Some(sock),
                output: TableOrJson::Json,
            }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_single_field_op_errors_when_the_field_is_missing() {
        // `print_field` bails when the reply lacks the expected field.
        let (_dir, sock, server) = fake_daemon_reply(json!({ "ok": true, "payload": {} }));
        let err = print_field(Some(sock), "token", "token").await.unwrap_err();
        assert!(err.to_string().contains("no `token` field"), "{err}");
        server.await.unwrap();
    }
}
