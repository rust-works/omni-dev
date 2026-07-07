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

use crate::cli::format::TableOrJson;
use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::DaemonEnvelope;
use crate::daemon::server;
use crate::snowflake::QueryRequest;
use crate::utils::env::EnvSource;
use crate::utils::settings::SettingsEnv;

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
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Json)]
    pub output: OutputFormat,
    /// Deprecated: use `-o`/`--output` instead.
    #[arg(long = "format", hide = true)]
    pub format: Option<OutputFormat>,
    /// SQL to run. Read from stdin when omitted.
    pub sql: Option<String>,
}

impl QueryCommand {
    /// Executes the query command.
    pub async fn execute(mut self) -> Result<()> {
        if let Some(format) = self.format.take() {
            eprintln!("warning: --format is deprecated; use -o/--output instead");
            self.output = format;
        }
        let sql = match self.sql.take() {
            Some(sql) => sql,
            None => read_stdin()?,
        };
        if sql.trim().is_empty() {
            bail!("no SQL provided (pass it as an argument or on stdin)");
        }

        let payload = serde_json::to_value(self.request(sql, &SettingsEnv::load()))?;

        // First-time auth for an (account, user) opens a browser; warn on stderr
        // so it doesn't pollute the JSON/YAML on stdout.
        eprintln!("snowflake: a browser may open for first-time sign-in…");

        let socket = server::resolve_socket(self.socket)?;
        let result = call(&socket, "query", payload).await?;
        print_value(&result, self.output)
    }

    /// Builds the query request, filling each unset identity/context field from
    /// `env` so `SNOWFLAKE_*` defaults — including profile-scoped ones selected
    /// via `--profile` / `OMNI_DEV_PROFILE` — resolve in this client process
    /// rather than against the daemon's startup environment (#1110).
    fn request(&self, sql: String, env: &impl EnvSource) -> QueryRequest {
        let mut req = QueryRequest {
            account: self.account.clone(),
            user: self.user.clone(),
            warehouse: self.warehouse.clone(),
            role: self.role.clone(),
            database: self.database.clone(),
            schema: self.schema.clone(),
            sql,
        };
        req.fill_defaults_from(env);
        req
    }
}

/// Lists the daemon's active multiplexed sessions.
#[derive(Parser)]
pub struct SessionsCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
    /// Deprecated: use `-o`/`--output json` instead.
    #[arg(long, hide = true)]
    pub json: bool,
}

impl SessionsCommand {
    /// Executes the sessions command.
    pub async fn execute(mut self) -> Result<()> {
        if self.json {
            eprintln!("warning: --json is deprecated; use -o/--output json instead");
            self.output = TableOrJson::Json;
        }
        let socket = server::resolve_socket(self.socket)?;
        let result = call(&socket, "sessions", Value::Null).await?;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            TableOrJson::Table => println!("{}", render_sessions(&result)),
        }
        Ok(())
    }
}

/// Renders a `sessions` reply as a human-readable table: a header, one row per
/// pool, and an indented line per authenticated session (what each is doing).
/// Returns a placeholder line when there are no active sessions.
fn render_sessions(result: &Value) -> String {
    let sessions = result
        .get("sessions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if sessions.is_empty() {
        return "No active sessions.".to_string();
    }
    let mut out = format!(
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
        out.push_str(&format!(
            "\n{id:<4} {account:<28} {user:<28} {pool:>8} {count:>7}"
        ));
        // One indented line per individual authenticated session (auth), with
        // what it's doing.
        if let Some(members) = session.get("members").and_then(Value::as_array) {
            for member in members {
                out.push_str(&format!("\n       {}", render_member(member)));
            }
        }
    }
    out
}

/// Renders one authenticated-session line: id, context, current state (running
/// query + elapsed, busy, or idle time), and lifetime query count.
fn render_member(member: &Value) -> String {
    let mid = member.get("id").and_then(Value::as_u64).unwrap_or(0);
    let qc = member
        .get("query_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let context = member
        .get("context")
        .map_or_else(|| "(default)".to_string(), context_summary);
    let state = if let Some(running) = member.get("running").filter(|r| !r.is_null()) {
        let sql = running.get("sql").and_then(Value::as_str).unwrap_or("");
        let secs = age_secs(running.get("started_at").and_then(Value::as_str));
        format!("running {secs}s: {sql}")
    } else if member.get("busy").and_then(Value::as_bool).unwrap_or(false) {
        "busy".to_string()
    } else {
        let secs = age_secs(member.get("last_used").and_then(Value::as_str));
        format!("idle {secs}s")
    };
    format!("#{mid} {context} · {state} · {qc} queries")
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
        let disconnected = result
            .get("disconnected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        println!(
            "{}",
            disconnect_message(disconnected, &self.account, &self.user)
        );
        Ok(())
    }
}

/// The message printed after a `disconnect`, depending on whether a session was
/// actually evicted.
fn disconnect_message(disconnected: bool, account: &str, user: &str) -> String {
    if disconnected {
        format!("Disconnected {account} / {user}.")
    } else {
        format!("No active session for {account} / {user}.")
    }
}

/// Sends one `snowflake` service op over the control socket, returning its
/// payload or turning an `ok: false` reply into an error.
///
/// The envelope carries this CLI process's request-log `invocation_id` so the
/// HTTP records the daemon writes while serving the op correlate back to this
/// invocation rather than the daemon's own (#1198).
async fn call(socket: &Path, op: &str, payload: Value) -> Result<Value> {
    let origin = crate::request_log::current_context().invocation_id;
    let reply = DaemonClient::new(socket)
        .request(DaemonEnvelope::service(SERVICE, op, payload).with_origin(origin))
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

/// Formats a JSON value in the requested output format.
fn format_value(value: &Value, format: OutputFormat) -> Result<String> {
    Ok(match format {
        OutputFormat::Json => serde_json::to_string_pretty(value)?,
        OutputFormat::Yaml => serde_yaml::to_string(value)?,
    })
}

/// Prints a JSON value in the requested format (JSON gets a trailing newline;
/// `serde_yaml` already emits one).
fn print_value(value: &Value, format: OutputFormat) -> Result<()> {
    let text = format_value(value, format)?;
    match format {
        OutputFormat::Json => println!("{text}"),
        OutputFormat::Yaml => print!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

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
            "-o",
            "yaml",
            "SELECT 1",
        ]) else {
            panic!("expected query");
        };
        assert_eq!(cmd.account.as_deref(), Some("ACCT"));
        assert_eq!(cmd.user.as_deref(), Some("me"));
        assert_eq!(cmd.warehouse.as_deref(), Some("WH"));
        assert_eq!(cmd.sql.as_deref(), Some("SELECT 1"));
        assert_eq!(cmd.output, OutputFormat::Yaml);
    }

    #[test]
    fn query_deprecated_format_alias_still_parses() {
        let SnowflakeSubcommands::Query(cmd) = parse(&["query", "--format", "yaml", "SELECT 1"])
        else {
            panic!("expected query");
        };
        // The deprecated `--format` is captured separately; `execute` folds it
        // into `output` with a stderr warning.
        assert_eq!(cmd.format, Some(OutputFormat::Yaml));
        assert_eq!(cmd.output, OutputFormat::Json);
    }

    #[test]
    fn query_sql_optional_and_format_defaults_to_json() {
        let SnowflakeSubcommands::Query(cmd) = parse(&["query"]) else {
            panic!("expected query");
        };
        assert!(cmd.sql.is_none());
        assert_eq!(cmd.output, OutputFormat::Json);
        assert!(cmd.format.is_none());
        assert!(cmd.socket.is_none());
    }

    #[test]
    fn query_request_resolves_defaults_from_env_source() {
        let SnowflakeSubcommands::Query(cmd) = parse(&["query", "SELECT 1"]) else {
            panic!("expected query");
        };
        let env = MapEnv::new()
            .with("SNOWFLAKE_ACCOUNT", "PROFILE_ACCT")
            .with("SNOWFLAKE_USER", "profile_user")
            .with("SNOWFLAKE_ROLE", "PROFILE_ROLE");
        let payload = serde_json::to_value(cmd.request("SELECT 1".to_string(), &env)).unwrap();
        assert_eq!(
            payload,
            json!({
                "account": "PROFILE_ACCT",
                "user": "profile_user",
                "role": "PROFILE_ROLE",
                "sql": "SELECT 1",
            })
        );
    }

    #[test]
    fn query_request_explicit_flags_beat_env_source() {
        let SnowflakeSubcommands::Query(cmd) =
            parse(&["query", "--account", "FLAG_ACCT", "SELECT 1"])
        else {
            panic!("expected query");
        };
        let env = MapEnv::new()
            .with("SNOWFLAKE_ACCOUNT", "ENV_ACCT")
            .with("SNOWFLAKE_USER", "env_user");
        let req = cmd.request("SELECT 1".to_string(), &env);
        assert_eq!(req.account.as_deref(), Some("FLAG_ACCT"));
        assert_eq!(req.user.as_deref(), Some("env_user"));
    }

    #[test]
    fn query_request_omits_unresolved_fields_from_payload() {
        let SnowflakeSubcommands::Query(cmd) = parse(&["query", "SELECT 1"]) else {
            panic!("expected query");
        };
        let payload =
            serde_json::to_value(cmd.request("SELECT 1".to_string(), &MapEnv::new())).unwrap();
        assert_eq!(payload, json!({ "sql": "SELECT 1" }));
    }

    #[test]
    fn sessions_output_flag_parses() {
        let SnowflakeSubcommands::Sessions(cmd) = parse(&["sessions", "-o", "json"]) else {
            panic!("expected sessions");
        };
        assert_eq!(cmd.output, TableOrJson::Json);
        assert!(!cmd.json);
    }

    #[test]
    fn sessions_deprecated_json_flag_still_parses() {
        let SnowflakeSubcommands::Sessions(cmd) = parse(&["sessions", "--json"]) else {
            panic!("expected sessions");
        };
        // Captured separately; `execute` folds it into `output` with a warning.
        assert!(cmd.json);
        assert_eq!(cmd.output, TableOrJson::Table);
    }

    #[tokio::test]
    async fn query_execute_folds_deprecated_format_flag() {
        let dir = tempfile::tempdir().unwrap();
        // Deprecated `--format` folds into `output` before the (absent-daemon)
        // socket call fails.
        let cmd = QueryCommand {
            account: None,
            user: None,
            warehouse: None,
            role: None,
            database: None,
            schema: None,
            socket: Some(dir.path().join("absent.sock")),
            output: OutputFormat::Json,
            format: Some(OutputFormat::Yaml),
            sql: Some("SELECT 1".to_string()),
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test]
    async fn sessions_execute_folds_deprecated_json_flag() {
        let dir = tempfile::tempdir().unwrap();
        // Deprecated `--json` folds into `output` before the (absent-daemon)
        // socket call fails.
        let cmd = SessionsCommand {
            socket: Some(dir.path().join("absent.sock")),
            output: TableOrJson::Table,
            json: true,
        };
        assert!(cmd.execute().await.is_err());
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

    #[test]
    fn age_secs_handles_absent_and_unparseable_and_past() {
        assert_eq!(age_secs(None), 0);
        assert_eq!(age_secs(Some("not-a-timestamp")), 0);
        assert!(age_secs(Some("2000-01-01T00:00:00Z")) > 0);
    }

    #[test]
    fn context_summary_joins_set_dimensions_or_default() {
        assert_eq!(context_summary(&json!({})), "(default)");
        assert_eq!(
            context_summary(&json!({ "warehouse": "WH", "role": "R" })),
            "WH/R"
        );
        assert_eq!(
            context_summary(&json!({ "warehouse": "WH", "database": "DB", "schema": "S" })),
            "WH/DB/S"
        );
    }

    #[test]
    fn render_sessions_handles_empty_replies() {
        assert_eq!(
            render_sessions(&json!({ "sessions": [] })),
            "No active sessions."
        );
        assert_eq!(render_sessions(&json!({})), "No active sessions.");
    }

    #[test]
    fn render_sessions_renders_running_busy_and_idle_members() {
        let result = json!({ "sessions": [{
            "id": 1, "account": "ACME", "user": "me",
            "sessions": 2, "max_sessions": 4, "query_count": 9,
            "members": [
                { "id": 1, "query_count": 3,
                  "context": { "warehouse": "WH", "role": "R" },
                  "running": { "sql": "SELECT 42", "started_at": "2000-01-01T00:00:00Z" } },
                { "id": 2, "query_count": 1, "context": {}, "busy": true },
                { "id": 3, "query_count": 0, "context": {}, "last_used": "2000-01-01T00:00:00Z" },
            ],
        }]});
        let table = render_sessions(&result);
        assert!(table.contains("ACME"), "{table}");
        assert!(table.contains("2/4"), "{table}");
        assert!(
            table.contains("running") && table.contains("SELECT 42"),
            "{table}"
        );
        assert!(table.contains("WH/R"), "{table}");
        assert!(table.contains("busy"), "{table}");
        assert!(table.contains("idle"), "{table}");
        assert!(table.contains("(default)"), "{table}");
    }

    #[test]
    fn format_value_renders_json_and_yaml() {
        let value = json!({ "a": 1 });
        assert!(format_value(&value, OutputFormat::Json)
            .unwrap()
            .contains("\"a\": 1"));
        assert!(format_value(&value, OutputFormat::Yaml)
            .unwrap()
            .contains("a: 1"));
    }

    #[test]
    fn disconnect_message_varies_on_outcome() {
        assert_eq!(
            disconnect_message(true, "ACME", "me"),
            "Disconnected ACME / me."
        );
        assert_eq!(
            disconnect_message(false, "ACME", "me"),
            "No active session for ACME / me."
        );
    }

    #[test]
    fn print_value_emits_both_formats() {
        // Exercises both arms; output goes to the test harness's captured stdout.
        print_value(&json!({ "a": 1 }), OutputFormat::Json).unwrap();
        print_value(&json!({ "a": 1 }), OutputFormat::Yaml).unwrap();
    }
}
