//! `omni-dev snowflake` — a thin client that runs SQL through the daemon's
//! multiplexed, authenticate-once Snowflake sessions.
//!
//! Lifecycle stays on `omni-dev daemon` (`start`/`stop`/`status`/`restart`);
//! these subcommands only send `query`/`sessions`/`disconnect` ops to the
//! `snowflake` service over the daemon's Unix control socket.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::DaemonEnvelope;
use crate::daemon::server;

/// The `snowflake` service routing key on the daemon control socket.
const SERVICE: &str = "snowflake";

/// Snowflake: authenticate once via external-browser SSO and run arbitrary SQL
/// across any account, multiplexed by the daemon.
#[derive(Parser)]
pub struct SnowflakeCommand {
    /// The Snowflake subcommand to execute.
    #[command(subcommand)]
    pub command: SnowflakeSubcommands,
}

/// Snowflake subcommands.
#[derive(Subcommand)]
pub enum SnowflakeSubcommands {
    /// Run SQL (from an argument or stdin) through a multiplexed session.
    Query(QueryCommand),
    /// List active multiplexed sessions.
    Sessions(SessionsCommand),
    /// Disconnect (evict) one session.
    Disconnect(DisconnectCommand),
}

impl SnowflakeCommand {
    /// Executes the Snowflake command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            SnowflakeSubcommands::Query(cmd) => cmd.execute().await,
            SnowflakeSubcommands::Sessions(cmd) => cmd.execute().await,
            SnowflakeSubcommands::Disconnect(cmd) => cmd.execute().await,
        }
    }
}

/// Output format for query results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Pretty-printed JSON (default).
    #[default]
    Json,
    /// YAML.
    Yaml,
}

/// Runs arbitrary SQL through the `(account, user)` session, authenticating it
/// on first use (a browser may open for sign-in).
#[derive(Parser)]
pub struct QueryCommand {
    /// Target account. Falls back to `SNOWFLAKE_ACCOUNT` / settings.json.
    #[arg(long)]
    pub account: Option<String>,
    /// Authenticating user. Falls back to `SNOWFLAKE_USER` / settings.json.
    #[arg(long)]
    pub user: Option<String>,
    /// Per-query warehouse (`USE WAREHOUSE`).
    #[arg(long)]
    pub warehouse: Option<String>,
    /// Per-query role (`USE ROLE`).
    #[arg(long)]
    pub role: Option<String>,
    /// Per-query database (`USE DATABASE`).
    #[arg(long)]
    pub database: Option<String>,
    /// Per-query schema (`USE SCHEMA`).
    #[arg(long)]
    pub schema: Option<String>,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub format: OutputFormat,
    /// SQL to run. Read from stdin when omitted.
    pub sql: Option<String>,
}

impl QueryCommand {
    /// Executes the query command.
    pub async fn execute(self) -> Result<()> {
        let sql = match self.sql {
            Some(sql) => sql,
            None => read_stdin()?,
        };
        if sql.trim().is_empty() {
            bail!("no SQL provided (pass it as an argument or on stdin)");
        }

        let payload = json!({
            "account": self.account,
            "user": self.user,
            "warehouse": self.warehouse,
            "role": self.role,
            "database": self.database,
            "schema": self.schema,
            "sql": sql,
        });

        // First-time auth for an (account, user) opens a browser; warn on stderr
        // so it doesn't pollute the JSON/YAML on stdout.
        eprintln!("snowflake: a browser may open for first-time sign-in…");

        let socket = server::resolve_socket(self.socket)?;
        let result = call(&socket, "query", payload).await?;
        print_value(&result, self.format)
    }
}

/// Lists the daemon's active multiplexed sessions.
#[derive(Parser)]
pub struct SessionsCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl SessionsCommand {
    /// Executes the sessions command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let result = call(&socket, "sessions", Value::Null).await?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        let sessions = result
            .get("sessions")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if sessions.is_empty() {
            println!("No active sessions.");
            return Ok(());
        }
        println!(
            "{:<4} {:<28} {:<28} {:>8} {:>7}",
            "ID", "ACCOUNT", "USER", "SESSIONS", "QUERIES"
        );
        for session in sessions {
            let id = session.get("id").and_then(Value::as_u64).unwrap_or(0);
            let account = session.get("account").and_then(Value::as_str).unwrap_or("");
            let user = session.get("user").and_then(Value::as_str).unwrap_or("");
            let live = session.get("sessions").and_then(Value::as_u64).unwrap_or(0);
            let max = session
                .get("max_sessions")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let count = session
                .get("query_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let pool = format!("{live}/{max}");
            println!("{id:<4} {account:<28} {user:<28} {pool:>8} {count:>7}");
            // One indented line per individual authenticated session (auth),
            // with what it's doing.
            if let Some(members) = session.get("members").and_then(Value::as_array) {
                for member in members {
                    let mid = member.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let qc = member
                        .get("query_count")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let context = member
                        .get("context")
                        .map_or_else(|| "(default)".to_string(), context_summary);
                    let state =
                        if let Some(running) = member.get("running").filter(|r| !r.is_null()) {
                            let sql = running.get("sql").and_then(Value::as_str).unwrap_or("");
                            let secs = age_secs(running.get("started_at").and_then(Value::as_str));
                            format!("running {secs}s: {sql}")
                        } else if member.get("busy").and_then(Value::as_bool).unwrap_or(false) {
                            "busy".to_string()
                        } else {
                            let secs = age_secs(member.get("last_used").and_then(Value::as_str));
                            format!("idle {secs}s")
                        };
                    println!("       #{mid} {context} · {state} · {qc} queries");
                }
            }
        }
        Ok(())
    }
}

/// Evicts a single multiplexed session.
#[derive(Parser)]
pub struct DisconnectCommand {
    /// Account of the session to evict.
    #[arg(long)]
    pub account: String,
    /// User of the session to evict.
    #[arg(long)]
    pub user: String,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl DisconnectCommand {
    /// Executes the disconnect command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let payload = json!({ "account": self.account, "user": self.user });
        let result = call(&socket, "disconnect", payload).await?;
        if result
            .get("disconnected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            println!("Disconnected {} / {}.", self.account, self.user);
        } else {
            println!("No active session for {} / {}.", self.account, self.user);
        }
        Ok(())
    }
}

/// Sends one `snowflake` service op over the control socket, returning its
/// payload or turning an `ok: false` reply into an error.
async fn call(socket: &Path, op: &str, payload: Value) -> Result<Value> {
    let reply = DaemonClient::new(socket)
        .request(DaemonEnvelope::service(SERVICE, op, payload))
        .await?;
    if reply.ok {
        Ok(reply.payload)
    } else {
        bail!(
            "daemon returned an error: {}",
            reply.error.as_deref().unwrap_or("unknown error")
        )
    }
}

/// Seconds elapsed since an RFC 3339 timestamp (0 if absent/unparseable).
fn age_secs(ts: Option<&str>) -> i64 {
    ts.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or(0, |t| {
            (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0)
        })
}

/// A compact `wh/role/db/schema` label from a serialized session context
/// (`(default)` when none are set).
fn context_summary(context: &Value) -> String {
    let parts: Vec<&str> = ["warehouse", "role", "database", "schema"]
        .iter()
        .filter_map(|key| context.get(*key).and_then(Value::as_str))
        .collect();
    if parts.is_empty() {
        "(default)".to_string()
    } else {
        parts.join("/")
    }
}

/// Reads SQL from stdin (the lumon pipe path).
fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read SQL from stdin")?;
    Ok(buf)
}

/// Prints a JSON value in the requested format.
fn print_value(value: &Value, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(value)?),
        OutputFormat::Yaml => print!("{}", serde_yaml::to_string(value)?),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Mirrors the `omni-dev snowflake` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: SnowflakeSubcommands,
    }

    fn parse(args: &[&str]) -> SnowflakeSubcommands {
        let mut full = vec!["omni-dev"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full).unwrap().cmd
    }

    #[test]
    fn query_parses_sql_and_flags() {
        let SnowflakeSubcommands::Query(cmd) = parse(&[
            "query",
            "--account",
            "ACCT",
            "--user",
            "me",
            "--warehouse",
            "WH",
            "--format",
            "yaml",
            "SELECT 1",
        ]) else {
            panic!("expected query");
        };
        assert_eq!(cmd.account.as_deref(), Some("ACCT"));
        assert_eq!(cmd.user.as_deref(), Some("me"));
        assert_eq!(cmd.warehouse.as_deref(), Some("WH"));
        assert_eq!(cmd.sql.as_deref(), Some("SELECT 1"));
        assert_eq!(cmd.format, OutputFormat::Yaml);
    }

    #[test]
    fn query_sql_optional_and_format_defaults_to_json() {
        let SnowflakeSubcommands::Query(cmd) = parse(&["query"]) else {
            panic!("expected query");
        };
        assert!(cmd.sql.is_none());
        assert_eq!(cmd.format, OutputFormat::Json);
        assert!(cmd.socket.is_none());
    }

    #[test]
    fn sessions_json_flag_parses() {
        let SnowflakeSubcommands::Sessions(cmd) = parse(&["sessions", "--json"]) else {
            panic!("expected sessions");
        };
        assert!(cmd.json);
    }

    #[test]
    fn disconnect_requires_account_and_user() {
        let SnowflakeSubcommands::Disconnect(cmd) =
            parse(&["disconnect", "--account", "ACCT", "--user", "me"])
        else {
            panic!("expected disconnect");
        };
        assert_eq!(cmd.account, "ACCT");
        assert_eq!(cmd.user, "me");

        // Missing required flags is a parse error.
        let mut full = vec!["omni-dev", "disconnect", "--account", "ACCT"];
        assert!(Wrapper::try_parse_from(std::mem::take(&mut full)).is_err());
    }
}
