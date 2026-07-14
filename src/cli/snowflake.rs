//! `omni-dev snowflake` — a thin client that runs SQL through the daemon's
//! multiplexed, authenticate-once Snowflake sessions.
//!
//! Lifecycle stays on `omni-dev daemon` (`start`/`stop`/`status`/`restart`);
//! these subcommands only send `query`/`sessions`/`disconnect` ops to the
//! `snowflake` service over the daemon's Unix control socket.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
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
    /// Cancel (abort) the running query on a pool without evicting the session:
    /// one `(account, user)` pool, a pool by id, or all pools.
    Cancel(CancelCommand),
    /// Disconnect (evict) sessions: one `(account, user)` pool, a pool by id, or
    /// all pools.
    Disconnect(DisconnectCommand),
}

impl SnowflakeCommand {
    /// Executes the Snowflake command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            SnowflakeSubcommands::Query(cmd) => cmd.execute().await,
            SnowflakeSubcommands::Sessions(cmd) => cmd.execute().await,
            SnowflakeSubcommands::Cancel(cmd) => cmd.execute().await,
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
    /// Comma-separated values (RFC 4180 quoting), one header row plus one row
    /// per result row.
    Csv,
    /// Tab-separated values (same quoting as CSV, tab delimiter).
    Tsv,
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
    /// Output file (writes to stdout if omitted).
    #[arg(long = "out-file", value_name = "PATH")]
    pub out_file: Option<String>,
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
        let text = format_output(&result, self.output)?;
        write_output(&text, self.out_file.as_deref())
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

/// Cancels (aborts) the running query on a target pool **without** evicting the
/// session.
///
/// Frees the pooled session promptly rather than waiting out
/// `SNOWFLAKE_QUERY_TIMEOUT`. Exactly one selector is required — the
/// `--account`/`--user` pair, `--id`, or `--all`. `--member` (a numeric member id
/// from `sessions`) narrows the cancel to one authenticated session's query; it
/// may not be combined with `--all`.
#[derive(Parser)]
#[command(group(
    ArgGroup::new("cancel-target")
        .required(true)
        .args(["account", "id", "all"]),
))]
pub struct CancelCommand {
    /// Account of the pool whose query to cancel (requires `--user`).
    #[arg(long, requires = "user")]
    pub account: Option<String>,
    /// User of the pool whose query to cancel (requires `--account`).
    #[arg(long, requires = "account")]
    pub user: Option<String>,
    /// Numeric id of the pool (as shown by `sessions`).
    #[arg(long)]
    pub id: Option<u64>,
    /// Cancel the running query on every pool.
    #[arg(long)]
    pub all: bool,
    /// Numeric member id (from `sessions`) to cancel a single session's query
    /// instead of every busy member in the pool. Not valid with `--all`.
    #[arg(long, conflicts_with = "all")]
    pub member: Option<u64>,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl CancelCommand {
    /// Executes the cancel command.
    pub async fn execute(mut self) -> Result<()> {
        let payload = self.payload();
        let socket = server::resolve_socket(self.socket.take())?;
        let result = call(&socket, "cancel", payload).await?;
        println!("{}", self.message(&result));
        Ok(())
    }

    /// Builds the socket payload for whichever selector was chosen, adding
    /// `member` when set. Exactly one selector is present (enforced by the
    /// `cancel-target` arg group).
    fn payload(&self) -> Value {
        let mut payload = if self.all {
            json!({ "all": true })
        } else if let Some(id) = self.id {
            json!({ "id": id })
        } else {
            json!({ "account": self.account, "user": self.user })
        };
        if let (Some(member), Some(map)) = (self.member, payload.as_object_mut()) {
            map.insert("member".to_string(), json!(member));
        }
        payload
    }

    /// The line printed after the socket call, matched to the selector, the
    /// optional member, and how many running queries were actually cancelled.
    fn message(&self, result: &Value) -> String {
        let cancelled = result.get("cancelled").and_then(Value::as_u64).unwrap_or(0);
        if self.all {
            return format!("Cancelled {cancelled} running query(ies) across all pools.");
        }
        let target = if let Some(id) = self.id {
            format!("pool #{id}")
        } else {
            // The arg group guarantees the pair when neither --all nor --id.
            let account = self.account.as_deref().unwrap_or_default();
            let user = self.user.as_deref().unwrap_or_default();
            format!("{account} / {user}")
        };
        cancel_message(cancelled, &target, self.member)
    }
}

/// The message printed after a targeted cancel, reflecting the scope (pool or
/// identity, optionally a single member) and how many queries were cancelled.
fn cancel_message(cancelled: u64, target: &str, member: Option<u64>) -> String {
    let scope = match member {
        Some(member) => format!("{target} member #{member}"),
        None => target.to_string(),
    };
    if cancelled == 0 {
        format!("No running query to cancel for {scope}.")
    } else {
        format!("Cancelled {cancelled} running query(ies) for {scope}.")
    }
}

/// Evicts multiplexed sessions: one `(account, user)` pool, a pool by numeric id
/// (from `sessions`), or every pool. Exactly one selector is required — the
/// `--account`/`--user` pair, `--id`, or `--all`.
#[derive(Parser)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .args(["account", "id", "all"]),
))]
pub struct DisconnectCommand {
    /// Account of the pool to evict (requires `--user`).
    #[arg(long, requires = "user")]
    pub account: Option<String>,
    /// User of the pool to evict (requires `--account`).
    #[arg(long, requires = "account")]
    pub user: Option<String>,
    /// Numeric id of the pool to evict (as shown by `sessions`).
    #[arg(long)]
    pub id: Option<u64>,
    /// Evict every pool.
    #[arg(long)]
    pub all: bool,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl DisconnectCommand {
    /// Executes the disconnect command.
    pub async fn execute(mut self) -> Result<()> {
        let payload = self.payload();
        let socket = server::resolve_socket(self.socket.take())?;
        let result = call(&socket, "disconnect", payload).await?;
        println!("{}", self.message(&result));
        Ok(())
    }

    /// Builds the socket payload for whichever selector was chosen. Exactly one
    /// is present (enforced by the `target` arg group), so the `--account`/
    /// `--user` fallthrough always has both.
    fn payload(&self) -> Value {
        if self.all {
            json!({ "all": true })
        } else if let Some(id) = self.id {
            json!({ "id": id })
        } else {
            json!({ "account": self.account, "user": self.user })
        }
    }

    /// The line printed after the socket call, matched to the selector and the
    /// number of pools the daemon actually evicted (`count`).
    fn message(&self, result: &Value) -> String {
        let count = result.get("count").and_then(Value::as_u64).unwrap_or(0);
        let disconnected = result
            .get("disconnected")
            .and_then(Value::as_bool)
            .unwrap_or(count > 0);
        if self.all {
            format!("Disconnected {count} session pool(s).")
        } else if let Some(id) = self.id {
            if disconnected {
                format!("Disconnected session pool #{id}.")
            } else {
                format!("No active session pool with id {id}.")
            }
        } else {
            // The arg group guarantees the pair when neither --all nor --id.
            let account = self.account.as_deref().unwrap_or_default();
            let user = self.user.as_deref().unwrap_or_default();
            disconnect_message(disconnected, account, user)
        }
    }
}

/// The message printed after a `(account, user)` disconnect, depending on whether
/// a session pool was actually evicted.
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

/// Renders a query result in the requested output format, returning the complete
/// text including its trailing newline so file and stdout writes are identical.
///
/// JSON/YAML serialize the whole `{columns, rows}` value; CSV/TSV instead consume
/// that shape directly so column order comes from `columns` (see
/// [`render_delimited`]).
fn format_output(value: &Value, format: OutputFormat) -> Result<String> {
    Ok(match format {
        // `to_string_pretty` has no trailing newline; add one for parity.
        OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(value)?),
        // `serde_yaml` already emits a trailing newline.
        OutputFormat::Yaml => serde_yaml::to_string(value)?,
        OutputFormat::Csv => render_delimited(value, ','),
        OutputFormat::Tsv => render_delimited(value, '\t'),
    })
}

/// Writes rendered output to `out_file`, or to stdout when it is `None`.
fn write_output(text: &str, out_file: Option<&str>) -> Result<()> {
    if let Some(path) = out_file {
        fs::write(path, text).with_context(|| format!("failed to write to {path}"))
    } else {
        print!("{text}");
        Ok(())
    }
}

/// Renders a `{columns, rows}` query payload as delimited text (`delim` is `,`
/// for CSV, `\t` for TSV): a header row of column names followed by one row per
/// result row, with RFC 4180 quoting and `\n` line terminators.
///
/// Column order and the header come from `columns[]`; each row's cells are fetched
/// by the same `_<n>`-disambiguated keys `Row::to_json_object` produces, so
/// repeated column names (e.g. `SELECT 1, 1`) are not collapsed. An empty result
/// (`columns: []`, which is all Snowflake reports for a zero-row query) renders as
/// an empty string.
fn render_delimited(value: &Value, delim: char) -> String {
    let names: Vec<&str> = value
        .get("columns")
        .and_then(Value::as_array)
        .map(|cols| {
            cols.iter()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return String::new();
    }

    // The lookup keys `to_json_object` used: the first occurrence of a name maps
    // to the bare name, later occurrences to `<name>_<n>`.
    let keys = disambiguated_keys(&names);

    let mut out = String::new();
    push_record(&mut out, names.iter().copied(), delim);

    let empty = Vec::new();
    let rows = value
        .get("rows")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for row in rows {
        let fields = keys.iter().map(|key| {
            row.get(key).map_or_else(String::new, |cell| {
                escape_field(&json_to_field(cell), delim)
            })
        });
        push_record(&mut out, fields, delim);
    }
    out
}

/// Reproduces `Row::to_json_object`'s duplicate-name disambiguation: the first
/// occurrence of a name stays bare, the `n`-th (n ≥ 2) becomes `<name>_<n>`.
fn disambiguated_keys(names: &[&str]) -> Vec<String> {
    let mut seen: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    names
        .iter()
        .map(|&name| {
            let count = seen.entry(name).or_insert(0);
            *count += 1;
            if *count == 1 {
                name.to_string()
            } else {
                format!("{name}_{count}")
            }
        })
        .collect()
}

/// Pushes one already-escaped record (delimiter-joined) plus a `\n` terminator.
/// Generic over `&str`/`String` fields so both the header and data rows share it.
fn push_record(out: &mut String, fields: impl Iterator<Item = impl AsRef<str>>, delim: char) {
    let mut first = true;
    for field in fields {
        if !first {
            out.push(delim);
        }
        out.push_str(field.as_ref());
        first = false;
    }
    out.push('\n');
}

/// Stringifies a JSON cell for a delimited field (before escaping): `null` → empty
/// string, strings verbatim, scalars via their JSON text, and `variant`/`object`/
/// `array` cells as compact JSON.
fn json_to_field(cell: &Value) -> String {
    match cell {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(_) | Value::Number(_) => cell.to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(cell).unwrap_or_else(|_| cell.to_string())
        }
    }
}

/// RFC 4180 field quoting: wrap in double quotes and double any embedded quotes
/// when the field contains the delimiter, a quote, or a newline/carriage return;
/// otherwise return it unchanged.
fn escape_field(field: &str, delim: char) -> String {
    if field.contains(delim) || field.contains(['"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
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
        try_parse(args).unwrap().cmd
    }

    fn try_parse(args: &[&str]) -> Result<Wrapper, clap::Error> {
        let mut full = vec!["omni-dev"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full)
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
            out_file: None,
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

    #[tokio::test]
    async fn cancel_execute_errors_without_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        // With no daemon at the socket path, `execute` builds the payload, resolves
        // the socket, and fails on the socket call.
        let cmd = CancelCommand {
            account: None,
            user: None,
            id: Some(3),
            all: false,
            member: Some(2),
            socket: Some(dir.path().join("absent.sock")),
        };
        assert!(cmd.execute().await.is_err());
    }

    #[test]
    fn cancel_selectors_and_member_parse_to_payloads() {
        let SnowflakeSubcommands::Cancel(cmd) =
            parse(&["cancel", "--account", "ACCT", "--user", "me"])
        else {
            panic!("expected cancel");
        };
        assert_eq!(cmd.payload(), json!({ "account": "ACCT", "user": "me" }));

        let SnowflakeSubcommands::Cancel(cmd) = parse(&["cancel", "--id", "3", "--member", "2"])
        else {
            panic!("expected cancel");
        };
        assert_eq!(cmd.payload(), json!({ "id": 3, "member": 2 }));

        let SnowflakeSubcommands::Cancel(cmd) = parse(&["cancel", "--all"]) else {
            panic!("expected cancel");
        };
        assert!(cmd.all);
        assert_eq!(cmd.payload(), json!({ "all": true }));

        // The pair form carries `member` too.
        let SnowflakeSubcommands::Cancel(cmd) =
            parse(&["cancel", "--account", "A", "--user", "u", "--member", "5"])
        else {
            panic!("expected cancel");
        };
        assert_eq!(
            cmd.payload(),
            json!({ "account": "A", "user": "u", "member": 5 })
        );
    }

    #[test]
    fn cancel_requires_one_selector_and_member_conflicts_with_all() {
        // No selector at all is an error (the `cancel-target` group is required).
        assert!(try_parse(&["cancel"]).is_err());
        // Selectors are mutually exclusive.
        assert!(try_parse(&["cancel", "--all", "--id", "1"]).is_err());
        assert!(try_parse(&["cancel", "--account", "A", "--user", "u", "--all"]).is_err());
        // --account without --user (and vice versa) is a parse error.
        assert!(try_parse(&["cancel", "--account", "ACCT"]).is_err());
        assert!(try_parse(&["cancel", "--user", "me"]).is_err());
        // --member cannot combine with --all.
        assert!(try_parse(&["cancel", "--all", "--member", "1"]).is_err());
    }

    #[test]
    fn cancel_message_reflects_selector_member_and_count() {
        let all = CancelCommand {
            account: None,
            user: None,
            id: None,
            all: true,
            member: None,
            socket: None,
        };
        assert_eq!(
            all.message(&json!({ "cancelled": 2 })),
            "Cancelled 2 running query(ies) across all pools."
        );

        let by_id = CancelCommand {
            account: None,
            user: None,
            id: Some(3),
            all: false,
            member: Some(2),
            socket: None,
        };
        assert_eq!(
            by_id.message(&json!({ "cancelled": 1 })),
            "Cancelled 1 running query(ies) for pool #3 member #2."
        );
        assert_eq!(
            by_id.message(&json!({ "cancelled": 0 })),
            "No running query to cancel for pool #3 member #2."
        );

        let pair = CancelCommand {
            account: Some("ACME".to_string()),
            user: Some("me".to_string()),
            id: None,
            all: false,
            member: None,
            socket: None,
        };
        assert_eq!(
            pair.message(&json!({ "cancelled": 1 })),
            "Cancelled 1 running query(ies) for ACME / me."
        );
    }

    #[test]
    fn disconnect_pair_parses_and_pairs_are_required_together() {
        let SnowflakeSubcommands::Disconnect(cmd) =
            parse(&["disconnect", "--account", "ACCT", "--user", "me"])
        else {
            panic!("expected disconnect");
        };
        assert_eq!(cmd.account.as_deref(), Some("ACCT"));
        assert_eq!(cmd.user.as_deref(), Some("me"));
        assert!(cmd.id.is_none());
        assert!(!cmd.all);
        assert_eq!(cmd.payload(), json!({ "account": "ACCT", "user": "me" }));

        // --account without --user (and vice versa) is a parse error.
        assert!(try_parse(&["disconnect", "--account", "ACCT"]).is_err());
        assert!(try_parse(&["disconnect", "--user", "me"]).is_err());
    }

    #[test]
    fn disconnect_by_id_and_all_selectors_parse() {
        let SnowflakeSubcommands::Disconnect(cmd) = parse(&["disconnect", "--id", "7"]) else {
            panic!("expected disconnect");
        };
        assert_eq!(cmd.id, Some(7));
        assert_eq!(cmd.payload(), json!({ "id": 7 }));

        let SnowflakeSubcommands::Disconnect(cmd) = parse(&["disconnect", "--all"]) else {
            panic!("expected disconnect");
        };
        assert!(cmd.all);
        assert_eq!(cmd.payload(), json!({ "all": true }));
    }

    #[test]
    fn disconnect_requires_and_conflicts_selectors() {
        // No selector at all is an error (the `target` group is required).
        assert!(try_parse(&["disconnect"]).is_err());
        // Selectors are mutually exclusive.
        assert!(try_parse(&["disconnect", "--all", "--id", "1"]).is_err());
        assert!(try_parse(&["disconnect", "--account", "A", "--user", "u", "--all"]).is_err());
        assert!(try_parse(&["disconnect", "--id", "1", "--account", "A", "--user", "u"]).is_err());
    }

    #[test]
    fn disconnect_message_reflects_selector_and_count() {
        let all = DisconnectCommand {
            account: None,
            user: None,
            id: None,
            all: true,
            socket: None,
        };
        assert_eq!(
            all.message(&json!({ "disconnected": true, "count": 3 })),
            "Disconnected 3 session pool(s)."
        );

        let by_id = DisconnectCommand {
            account: None,
            user: None,
            id: Some(5),
            all: false,
            socket: None,
        };
        assert_eq!(
            by_id.message(&json!({ "disconnected": true, "count": 1 })),
            "Disconnected session pool #5."
        );
        assert_eq!(
            by_id.message(&json!({ "disconnected": false, "count": 0 })),
            "No active session pool with id 5."
        );
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
    fn format_output_renders_json_and_yaml() {
        let value = json!({ "a": 1 });
        let json = format_output(&value, OutputFormat::Json).unwrap();
        assert!(json.contains("\"a\": 1"));
        assert!(
            json.ends_with("}\n"),
            "JSON gets a trailing newline: {json:?}"
        );
        let yaml = format_output(&value, OutputFormat::Yaml).unwrap();
        assert!(yaml.contains("a: 1"));
        assert!(yaml.ends_with('\n'), "YAML ends in a newline: {yaml:?}");
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
    fn query_parses_csv_tsv_and_out_file() {
        let SnowflakeSubcommands::Query(cmd) =
            parse(&["query", "-o", "csv", "--out-file", "rows.csv", "SELECT 1"])
        else {
            panic!("expected query");
        };
        assert_eq!(cmd.output, OutputFormat::Csv);
        assert_eq!(cmd.out_file.as_deref(), Some("rows.csv"));

        let SnowflakeSubcommands::Query(cmd) = parse(&["query", "-o", "tsv", "SELECT 1"]) else {
            panic!("expected query");
        };
        assert_eq!(cmd.output, OutputFormat::Tsv);
        assert!(cmd.out_file.is_none());
    }

    /// A two-column, one-row payload in the daemon's self-describing shape.
    fn sample_payload() -> Value {
        json!({
            "columns": [{ "name": "ID", "type": "fixed(38,0)" },
                        { "name": "NAME", "type": "text(16777216)" }],
            "rows": [{ "ID": 1, "NAME": "hello" }],
        })
    }

    #[test]
    fn render_delimited_writes_header_then_rows() {
        assert_eq!(
            render_delimited(&sample_payload(), ','),
            "ID,NAME\n1,hello\n"
        );
        assert_eq!(
            render_delimited(&sample_payload(), '\t'),
            "ID\tNAME\n1\thello\n"
        );
    }

    #[test]
    fn render_delimited_orders_columns_from_columns_array() {
        // Row object key order must not matter: `columns[]` drives order.
        let payload = json!({
            "columns": [{ "name": "B" }, { "name": "A" }],
            "rows": [{ "A": 1, "B": 2 }],
        });
        assert_eq!(render_delimited(&payload, ','), "B,A\n2,1\n");
    }

    #[test]
    fn render_delimited_quotes_per_rfc4180() {
        let payload = json!({
            "columns": [{ "name": "C" }],
            "rows": [
                { "C": "a,b" },
                { "C": "he said \"hi\"" },
                { "C": "line1\nline2" },
                { "C": "plain" },
            ],
        });
        assert_eq!(
            render_delimited(&payload, ','),
            "C\n\"a,b\"\n\"he said \"\"hi\"\"\"\n\"line1\nline2\"\nplain\n"
        );
        // A comma is not special in TSV, so `a,b` stays unquoted there…
        assert!(render_delimited(&payload, '\t').contains("\na,b\n"));
    }

    #[test]
    fn render_delimited_renders_null_variant_and_scalars() {
        let payload = json!({
            "columns": [{ "name": "N" }, { "name": "B" }, { "name": "V" }],
            "rows": [{ "N": null, "B": true, "V": { "a": 1 } }],
        });
        // null → empty field; bool → literal; object → compact JSON (quoted for
        // its embedded comma).
        assert_eq!(
            render_delimited(&payload, ','),
            "N,B,V\n,true,\"{\"\"a\"\":1}\"\n"
        );
    }

    #[test]
    fn render_delimited_disambiguates_duplicate_columns() {
        // `SELECT 1, 1` → columns [N, N]; row keys are N and N_2.
        let payload = json!({
            "columns": [{ "name": "N" }, { "name": "N" }],
            "rows": [{ "N": 1, "N_2": 2 }],
        });
        // Header keeps the original names; both cells survive.
        assert_eq!(render_delimited(&payload, ','), "N,N\n1,2\n");
    }

    #[test]
    fn render_delimited_empty_result_is_empty() {
        // A zero-row query reports `columns: []`; there is nothing to render.
        assert_eq!(
            render_delimited(&json!({ "columns": [], "rows": [] }), ','),
            ""
        );
        assert_eq!(render_delimited(&json!({}), ','), "");
    }

    #[test]
    fn format_output_dispatches_to_delimited() {
        assert_eq!(
            format_output(&sample_payload(), OutputFormat::Csv).unwrap(),
            "ID,NAME\n1,hello\n"
        );
        assert_eq!(
            format_output(&sample_payload(), OutputFormat::Tsv).unwrap(),
            "ID\tNAME\n1\thello\n"
        );
    }

    #[test]
    fn escape_field_quotes_only_when_needed() {
        assert_eq!(escape_field("plain", ','), "plain");
        assert_eq!(escape_field("a,b", ','), "\"a,b\"");
        assert_eq!(escape_field("a,b", '\t'), "a,b");
        assert_eq!(escape_field("a\"b", ','), "\"a\"\"b\"");
        assert_eq!(escape_field("a\nb", '\t'), "\"a\nb\"");
    }

    #[test]
    fn write_output_to_file_and_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.csv");
        write_output("ID\n1\n", Some(path.to_str().unwrap())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ID\n1\n");
        // The stdout branch just needs to not error.
        write_output("noop", None).unwrap();
    }

    #[test]
    fn write_output_invalid_path_errors() {
        let err = write_output("x", Some("/nonexistent_dir_for_test/out.csv")).unwrap_err();
        assert!(err.to_string().contains("failed to write"));
    }
}
