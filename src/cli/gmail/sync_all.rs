//! CLI command for `omni-dev gmail sync-all` (issue #1504,
//! [ADR-0068](../../../docs/adrs/adr-0068.md)).
//!
//! Fans a `.omni-dev/gmail-sync.yaml`-configured list of named accounts out
//! to `gmail sync`'s unchanged engine (`super::sync::engine`) concurrently,
//! each resolved via [`super::helpers::create_client_for`] (never the
//! `--account`-driven env var, which is unsafe across concurrent tasks) and
//! bounded by one shared `Semaphore`-based fetch cap layered underneath
//! each account's own local `--concurrency`. See ADR-0068 for the full
//! rationale; this file is CLI glue plus the config loader, mirroring how
//! `sync.rs` relates to `sync/engine.rs`.
//!
//! On an interactive terminal (same [`super::sync::should_show_progress`]
//! gate `gmail sync` uses), every account also gets its own listing-spinner
//! and fetch-bar pair, all registered on one shared `indicatif::MultiProgress`
//! via [`super::sync::progress::SyncProgressBars::new_in`]. ADR-0068's
//! Decision 4 originally deferred this (N accounts' independent
//! `MultiProgress`es would fight over one stderr); a single shared instance
//! sidesteps that entirely.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{FuturesUnordered, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Semaphore};

use crate::claude::context::discovery;
use crate::gmail::account::validate_account;
use crate::gmail::client::GmailClient;
use crate::utils::settings::Settings;

use super::format::{output_as, write_scalar_jsonl, JsonlSerialize, OutputFormat};
use super::helpers;
use super::sync::engine::{self, SyncOptions};
use super::sync::progress::{SyncProgressBars, SyncProgressEvent};
use super::sync::report::{SyncAction, SyncError, SyncReport, SyncSummary};
use super::sync::{format_summary_line, should_show_progress, DEFAULT_SYNC_CONCURRENCY};

/// `.omni-dev/gmail-sync.yaml`'s top level.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct GmailSyncAllConfig {
    /// A fetch-request cap shared across every account's fan-out this run.
    /// Overridden by `sync-all --concurrency`; falls back to
    /// [`DEFAULT_SYNC_CONCURRENCY`] when neither is set.
    #[serde(default)]
    pub(crate) concurrency: Option<usize>,
    /// The accounts to sync. Required to be non-empty — see
    /// [`load_gmail_sync_config`].
    #[serde(default)]
    pub(crate) accounts: Vec<GmailSyncAccountEntry>,
}

/// One `gmail-sync.yaml` account entry.
#[derive(Debug, Deserialize)]
pub(crate) struct GmailSyncAccountEntry {
    /// A lookup key into `~/.omni-dev/settings.json`'s `gmail.accounts` map
    /// (ADR-0066) — never a second credential store.
    pub(crate) account: String,
    /// Where to archive this account, relative to the project root (the
    /// parent of the discovered `.omni-dev/`) unless absolute.
    pub(crate) output_dir: PathBuf,
    /// Mirrors `gmail sync --query`, applied only to this account.
    #[serde(default)]
    pub(crate) query: Option<String>,
    /// Mirrors `gmail sync --extract-attachments`, applied only to this
    /// account. `None` behaves as `false`.
    #[serde(default)]
    pub(crate) extract_attachments: Option<bool>,
}

/// Loads and validates `gmail-sync.yaml` strictly: unlike `scopes.yaml`/
/// `commit-rules.yaml`'s warn-and-fall-back-to-default loaders, a missing
/// file, an empty `accounts` list, or malformed YAML are all hard errors
/// here, before any network call is made — a silently-empty account list
/// would otherwise look like a successful no-op (ADR-0068).
pub(crate) fn load_gmail_sync_config(context_dir: &Path) -> Result<GmailSyncAllConfig> {
    let path = discovery::resolve_config_file(context_dir, "gmail-sync.yaml");
    if !path.exists() {
        anyhow::bail!(
            "no gmail-sync.yaml found (looked under {}); add one with an `accounts:` list, or \
             point --context-dir/OMNI_DEV_CONFIG_DIR at a directory containing one",
            context_dir.display()
        );
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let config: GmailSyncAllConfig = serde_yaml::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    if config.accounts.is_empty() {
        anyhow::bail!(
            "{} has no accounts configured; add entries to its `accounts:` list",
            path.display()
        );
    }
    Ok(config)
}

/// Fails fast, before any client is built, if `gmail-sync.yaml` names an
/// account `~/.omni-dev/settings.json` doesn't know about — batches every
/// unknown name into one error rather than discovering them one task
/// failure at a time.
fn validate_accounts(settings: &Settings, accounts: &[GmailSyncAccountEntry]) -> Result<()> {
    let unknown: Vec<&str> = accounts
        .iter()
        .filter(|entry| validate_account(&settings.gmail, &entry.account).is_err())
        .map(|entry| entry.account.as_str())
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "gmail-sync.yaml references unknown Gmail account(s): {}. Run `gmail account list` to \
         see configured accounts.",
        unknown.join(", ")
    );
}

/// Maintains durable local archives for every account configured in
/// `.omni-dev/gmail-sync.yaml`, concurrently (no MCP equivalent — see
/// `gmail sync`'s own doc comment; the same bulk-filesystem-operation
/// reasoning applies doubly here).
#[derive(Parser)]
pub struct SyncAllCommand {
    /// Overrides the standard `.omni-dev/` discovery (walk-up from CWD,
    /// `OMNI_DEV_CONFIG_DIR`, `local/` shadow) used to locate
    /// `gmail-sync.yaml`.
    #[arg(long, value_name = "PATH")]
    pub context_dir: Option<PathBuf>,

    /// Overrides `gmail-sync.yaml`'s `concurrency` — a fetch-request cap
    /// shared across every configured account this run. Left unset, the
    /// YAML value applies; if that's unset too, falls back to the same
    /// default as `gmail sync --concurrency`.
    #[arg(long, value_name = "N")]
    pub concurrency: Option<usize>,

    /// Forces a full backfill/reconciliation pass for every account, same
    /// as `gmail sync --full`.
    #[arg(long)]
    pub full: bool,

    /// Reports the planned actions for every account without writing any
    /// files.
    #[arg(long)]
    pub dry_run: bool,

    /// Suppresses the per-account summary line printed as each account
    /// finishes, and the live per-account progress bars a run shows on an
    /// interactive terminal; per-message errors and the trailing combined
    /// total still print regardless.
    #[arg(long)]
    pub quiet: bool,

    /// Report format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// One account's outcome for `-o json`/`yaml`/`jsonl` rendering. A task
/// that failed before producing a [`SyncReport`] at all (bad credentials, a
/// rejected output directory, …) carries `account_error` instead of an
/// empty report — never silently folded into a zero-actions success.
#[derive(Serialize)]
struct AccountSyncOutcome {
    account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_error: Option<String>,
    actions: Vec<SyncAction>,
    errors: Vec<SyncError>,
    summary: SyncSummary,
}

/// `-o json`/`yaml`/`yamls`/`jsonl` view of a whole `sync-all` run.
#[derive(Serialize)]
struct SyncAllReportOutput {
    accounts: Vec<AccountSyncOutcome>,
    combined_summary: SyncSummary,
}

impl JsonlSerialize for SyncAllReportOutput {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Resolves an account name to a client for one `run_sync_all` call.
///
/// Production always uses [`helpers::create_client_for`]; tests inject a
/// closure over pre-built wiremock-backed clients instead, the same split
/// `sync.rs::run_sync_command` uses to exercise its own logic without a
/// real credential-loading path (`helpers::create_client_for` always
/// targets the real Gmail API host — see its own doc comment).
type ClientFor = Arc<dyn Fn(&str) -> Result<GmailClient> + Send + Sync>;

impl SyncAllCommand {
    pub(crate) async fn execute(self) -> Result<()> {
        let (context_dir, _source) =
            discovery::resolve_context_dir_with_source(self.context_dir.as_deref());
        let config = load_gmail_sync_config(&context_dir)?;

        let settings = Settings::load().context("Failed to load ~/.omni-dev/settings.json")?;
        validate_accounts(&settings, &config.accounts)?;

        let effective_concurrency = self
            .concurrency
            .or(config.concurrency)
            .unwrap_or(DEFAULT_SYNC_CONCURRENCY)
            .max(1);

        let project_root: PathBuf = context_dir
            .parent()
            .map_or_else(|| context_dir.clone(), Path::to_path_buf);

        let client_for: ClientFor =
            Arc::new(|account: &str| helpers::create_client_for(Some(account)));

        run_sync_all(
            &config,
            &project_root,
            self.full,
            self.dry_run,
            self.quiet,
            &self.output,
            effective_concurrency,
            client_for,
        )
        .await
    }
}

/// The orchestration core, split out from [`SyncAllCommand::execute`] so
/// tests can inject a [`ClientFor`] over wiremock-backed clients instead of
/// going through real credential resolution — mirrors
/// `sync.rs::run_sync_command`'s split from `SyncCommand::execute`.
#[allow(clippy::too_many_arguments)]
async fn run_sync_all(
    config: &GmailSyncAllConfig,
    project_root: &Path,
    full: bool,
    dry_run: bool,
    quiet: bool,
    output: &OutputFormat,
    effective_concurrency: usize,
    client_for: ClientFor,
) -> Result<()> {
    let pool = Arc::new(Semaphore::new(effective_concurrency));

    // Same gate `gmail sync` uses (`should_show_progress`): only stand up
    // live bars when a human is actually watching a `-o table` run on an
    // interactive stderr. One shared `MultiProgress` — rather than each
    // account's own, which would fight over the same stderr — is what makes
    // rendering N accounts' bars concurrently safe (ADR-0068's Decision 4
    // follow-up, #1504).
    let show_progress = should_show_progress(quiet, output, std::io::stderr().is_terminal());
    let multi = show_progress.then(indicatif::MultiProgress::new);
    let mut render_tasks = Vec::new();

    let mut tasks = FuturesUnordered::new();
    for entry in &config.accounts {
        let account = entry.account.clone();
        let output_dir = if entry.output_dir.is_relative() {
            project_root.join(&entry.output_dir)
        } else {
            entry.output_dir.clone()
        };
        let opts = SyncOptions {
            output_dir,
            query: entry.query.clone(),
            full,
            concurrency: DEFAULT_SYNC_CONCURRENCY,
            dry_run,
            extract_attachments: entry.extract_attachments.unwrap_or(false),
            shared_pool: Some(Arc::clone(&pool)),
        };
        let progress_tx = multi.as_ref().map(|multi| {
            let bars = SyncProgressBars::new_in(multi, &account);
            let (tx, rx) = mpsc::unbounded_channel();
            render_tasks.push(tokio::spawn(bars.drain(rx)));
            tx
        });
        let spawn_account = account.clone();
        let spawn_client_for = Arc::clone(&client_for);
        let join = tokio::spawn(async move {
            run_one_account(
                &spawn_account,
                &opts,
                spawn_client_for.as_ref(),
                progress_tx.as_ref(),
            )
            .await
            // `progress_tx` drops here, closing its channel so the
            // corresponding `bars.drain` render task above can exit.
        });
        tasks.push(async move {
            let outcome = match join.await {
                Ok(result) => result,
                Err(join_err) => Err(anyhow::anyhow!("sync task panicked: {join_err}")),
            };
            (account, outcome)
        });
    }

    let show_text = matches!(output, OutputFormat::Table);
    let mut outcomes: Vec<(String, Result<SyncReport>)> = Vec::with_capacity(config.accounts.len());
    while let Some((account, outcome)) = tasks.next().await {
        if show_text {
            // While bars are live, print through `suspend` so this stdout
            // line doesn't land mid-redraw of the stderr bars below it —
            // `suspend` clears every registered bar first and redraws once
            // the closure returns, regardless of which stream it writes to.
            match &multi {
                Some(multi) => multi.suspend(|| print_account_line(&account, &outcome, quiet)),
                None => print_account_line(&account, &outcome, quiet),
            }
        }
        outcomes.push((account, outcome));
    }

    // Best-effort: a panicking/cancelled render task shouldn't fail an
    // otherwise-successful sync — it only ever formats bars. Awaited before
    // the combined summary prints so every bar has finished/cleared first.
    for render_task in render_tasks {
        let _ = render_task.await;
    }

    let mut combined = SyncSummary::default();
    let mut any_failed = false;
    let mut accounts_output = Vec::with_capacity(outcomes.len());
    for (account, outcome) in outcomes {
        match outcome {
            Ok(report) => {
                let summary = report.summary();
                if !report.errors.is_empty() {
                    any_failed = true;
                }
                add_summary(&mut combined, &summary);
                accounts_output.push(AccountSyncOutcome {
                    account,
                    account_error: None,
                    actions: report.actions,
                    errors: report.errors,
                    summary,
                });
            }
            Err(err) => {
                any_failed = true;
                combined.errors += 1;
                accounts_output.push(AccountSyncOutcome {
                    account,
                    account_error: Some(format!("{err:#}")),
                    actions: Vec::new(),
                    errors: Vec::new(),
                    summary: SyncSummary {
                        errors: 1,
                        ..SyncSummary::default()
                    },
                });
            }
        }
    }

    if show_text {
        println!("combined: {}", format_summary_line(&combined));
    } else {
        output_as(
            &SyncAllReportOutput {
                accounts: accounts_output,
                combined_summary: combined,
            },
            output,
        )?;
    }

    if any_failed {
        anyhow::bail!("one or more accounts failed to sync; see output above");
    }
    Ok(())
}

/// Runs one account's sync from its own client, wrapping a client-build
/// failure and an `engine::run_sync_with_progress` failure in the same
/// `Result` so the caller treats both as one task-level failure — see
/// [`run_sync_all`]'s `any_failed` handling. `progress` is `None` whenever
/// [`run_sync_all`] didn't stand up live bars for this run (`--quiet`, a
/// non-table `-o` format, or a non-interactive stderr).
async fn run_one_account(
    account: &str,
    opts: &SyncOptions,
    client_for: &(dyn Fn(&str) -> Result<GmailClient> + Send + Sync),
    progress: Option<&mpsc::UnboundedSender<SyncProgressEvent>>,
) -> Result<SyncReport> {
    let client = client_for(account)
        .with_context(|| format!("account '{account}': failed to build a Gmail client"))?;
    engine::run_sync_with_progress(&client, opts, progress)
        .await
        .with_context(|| format!("account '{account}': sync failed"))
}

/// Prints one account's outcome as it finishes: per-message errors always
/// show (never suppressed by `--quiet`, mirroring `gmail sync`'s own
/// treatment of errors), the routine summary line is suppressed under
/// `--quiet`, and a task-level failure always shows.
fn print_account_line(account: &str, outcome: &Result<SyncReport>, quiet: bool) {
    match outcome {
        Ok(report) => {
            for error in &report.errors {
                println!("{account}: error: {} failed: {}", error.id, error.reason);
            }
            if !quiet {
                println!("{account}: {}", format_summary_line(&report.summary()));
            }
        }
        Err(err) => println!("{account}: failed: {err:#}"),
    }
}

/// Folds one account's summary into a running combined total, field by
/// field — [`SyncSummary`] has no `Add` impl since nothing else in
/// `gmail sync` needs to combine two of them.
fn add_summary(total: &mut SyncSummary, summary: &SyncSummary) {
    total.fetched += summary.fetched;
    total.would_fetch += summary.would_fetch;
    total.labels_updated += summary.labels_updated;
    total.deleted += summary.deleted;
    total.undeleted += summary.undeleted;
    total.would_delete += summary.would_delete;
    total.would_undelete += summary.would_undelete;
    total.errors += summary.errors;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use base64::Engine as _;

    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;
    use crate::utils::settings::GmailAccountSettings;

    use super::*;

    // ── load_gmail_sync_config / validate_accounts ───────────────────────

    #[test]
    fn load_gmail_sync_config_parses_accounts_and_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gmail-sync.yaml"),
            r#"
concurrency: 5
accounts:
  - account: jky.greens
    output_dir: emails/jky.greens/
  - account: newhoggy
    output_dir: emails/newhoggy/
    query: "-in:spam"
    extract_attachments: true
"#,
        )
        .unwrap();

        let config = load_gmail_sync_config(dir.path()).unwrap();
        assert_eq!(config.concurrency, Some(5));
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.accounts[0].account, "jky.greens");
        assert_eq!(config.accounts[1].query.as_deref(), Some("-in:spam"));
        assert_eq!(config.accounts[1].extract_attachments, Some(true));
    }

    #[test]
    fn load_gmail_sync_config_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_gmail_sync_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no gmail-sync.yaml found"));
    }

    #[test]
    fn load_gmail_sync_config_empty_accounts_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gmail-sync.yaml"), "accounts: []\n").unwrap();
        let err = load_gmail_sync_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no accounts configured"));
    }

    #[test]
    fn load_gmail_sync_config_malformed_yaml_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gmail-sync.yaml"), "accounts: [not valid").unwrap();
        assert!(load_gmail_sync_config(dir.path()).is_err());
    }

    #[test]
    fn load_gmail_sync_config_prefers_local_shadow_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gmail-sync.yaml"),
            "accounts:\n  - account: shared\n    output_dir: emails/shared/\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("local")).unwrap();
        std::fs::write(
            dir.path().join("local").join("gmail-sync.yaml"),
            "accounts:\n  - account: personal\n    output_dir: emails/personal/\n",
        )
        .unwrap();

        let config = load_gmail_sync_config(dir.path()).unwrap();
        assert_eq!(config.accounts[0].account, "personal");
    }

    fn account_entry(account: &str) -> GmailSyncAccountEntry {
        GmailSyncAccountEntry {
            account: account.to_string(),
            output_dir: PathBuf::from("out"),
            query: None,
            extract_attachments: None,
        }
    }

    #[test]
    fn validate_accounts_batches_every_unknown_name() {
        let mut settings = Settings::default();
        settings
            .gmail
            .accounts
            .insert("known".to_string(), GmailAccountSettings::default());
        let entries = vec![
            account_entry("known"),
            account_entry("missing1"),
            account_entry("missing2"),
        ];

        let err = validate_accounts(&settings, &entries).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("missing1"));
        assert!(message.contains("missing2"));
        assert!(!message.contains("'known'"));
    }

    #[test]
    fn validate_accounts_ok_when_all_known() {
        let mut settings = Settings::default();
        settings
            .gmail
            .accounts
            .insert("known".to_string(), GmailAccountSettings::default());
        assert!(validate_accounts(&settings, &[account_entry("known")]).is_ok());
    }

    // ── run_sync_all end-to-end (ADR-0068) ────────────────────────────────

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
        }
    }

    async fn mount_token(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;
    }

    /// A client pointed at `server`, independent of every other client
    /// built this way — two "accounts" sharing one mock server (this file
    /// doesn't need per-account mailbox content to prove the
    /// orchestration: two distinct clients, two distinct output
    /// directories, run concurrently, aggregated correctly).
    fn bootstrapped_client(server: &wiremock::MockServer) -> GmailClient {
        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    async fn mount_profile(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": "user@example.com",
                    "messagesTotal": 1,
                    "threadsTotal": 1,
                    "historyId": "500",
                })),
            )
            .mount(server)
            .await;
    }

    async fn mount_message_list(server: &wiremock::MockServer, ids: &[&str]) {
        let messages: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({"id": id, "threadId": "t1"}))
            .collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"messages": messages})),
            )
            .mount(server)
            .await;
    }

    async fn mount_raw_get(server: &wiremock::MockServer, id: &str) {
        let source = format!("Subject: Hello\r\nFrom: a@example.com\r\n\r\nBody of {id}.");
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/gmail/v1/users/me/messages/{id}"
            )))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "threadId": "t1", "labelIds": ["INBOX"],
                    "internalDate": "1700000000000", "historyId": "500",
                    "raw": raw,
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_sync_all_syncs_every_configured_account_concurrently() {
        let server = wiremock::MockServer::start().await;
        mount_token(&server).await;
        mount_profile(&server).await;
        mount_message_list(&server, &["m1", "m2"]).await;
        mount_raw_get(&server, "m1").await;
        mount_raw_get(&server, "m2").await;

        let client_a = bootstrapped_client(&server);
        let client_b = bootstrapped_client(&server);

        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path();
        let output_a = project_root.join("emails/acct-a");
        let output_b = project_root.join("emails/acct-b");

        let config = GmailSyncAllConfig {
            concurrency: None,
            accounts: vec![
                GmailSyncAccountEntry {
                    account: "acct-a".to_string(),
                    output_dir: output_a.clone(),
                    query: None,
                    extract_attachments: None,
                },
                GmailSyncAccountEntry {
                    account: "acct-b".to_string(),
                    output_dir: output_b.clone(),
                    query: None,
                    extract_attachments: None,
                },
            ],
        };

        let clients = Mutex::new(HashMap::from([
            ("acct-a".to_string(), client_a),
            ("acct-b".to_string(), client_b),
        ]));
        let clients = std::sync::Arc::new(clients);
        let client_for: ClientFor = std::sync::Arc::new(move |account: &str| {
            clients
                .lock()
                .unwrap()
                .remove(account)
                .ok_or_else(|| anyhow::anyhow!("no test client for {account}"))
        });

        run_sync_all(
            &config,
            project_root,
            false,
            false,
            true,
            &OutputFormat::Table,
            20,
            client_for,
        )
        .await
        .unwrap();

        for output_dir in [&output_a, &output_b] {
            let manifest = std::fs::read_to_string(output_dir.join("manifest.jsonl")).unwrap();
            assert_eq!(
                manifest.lines().count(),
                2,
                "{} should have archived both messages",
                output_dir.display()
            );
        }
    }

    #[tokio::test]
    async fn run_sync_all_fails_overall_but_still_syncs_the_other_account() {
        let server = wiremock::MockServer::start().await;
        mount_token(&server).await;
        mount_profile(&server).await;
        mount_message_list(&server, &["m1"]).await;
        mount_raw_get(&server, "m1").await;

        let client_good = bootstrapped_client(&server);

        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path();
        let output_good = project_root.join("good");

        let config = GmailSyncAllConfig {
            concurrency: None,
            accounts: vec![
                GmailSyncAccountEntry {
                    account: "good".to_string(),
                    output_dir: output_good.clone(),
                    query: None,
                    extract_attachments: None,
                },
                GmailSyncAccountEntry {
                    account: "broken".to_string(),
                    output_dir: project_root.join("broken"),
                    query: None,
                    extract_attachments: None,
                },
            ],
        };

        let client_good = Mutex::new(Some(client_good));
        let client_for: ClientFor = std::sync::Arc::new(move |account: &str| {
            if account == "good" {
                client_good
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("client used twice"))
            } else {
                Err(anyhow::anyhow!(
                    "simulated credential failure for {account}"
                ))
            }
        });

        let err = run_sync_all(
            &config,
            project_root,
            false,
            false,
            true,
            &OutputFormat::Table,
            20,
            client_for,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("one or more accounts failed"));

        // The failing account never blocked or aborted the working one.
        let manifest = std::fs::read_to_string(output_good.join("manifest.jsonl")).unwrap();
        assert_eq!(manifest.lines().count(), 1);
    }

    #[tokio::test]
    async fn run_sync_all_survives_a_spawned_task_panic_and_still_syncs_the_other_account() {
        let server = wiremock::MockServer::start().await;
        mount_token(&server).await;
        mount_profile(&server).await;
        mount_message_list(&server, &["m1"]).await;
        mount_raw_get(&server, "m1").await;

        let client_good = bootstrapped_client(&server);

        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path();
        let output_good = project_root.join("good");

        let config = GmailSyncAllConfig {
            concurrency: None,
            accounts: vec![
                GmailSyncAccountEntry {
                    account: "good".to_string(),
                    output_dir: output_good.clone(),
                    query: None,
                    extract_attachments: None,
                },
                GmailSyncAccountEntry {
                    account: "boom".to_string(),
                    output_dir: project_root.join("boom"),
                    query: None,
                    extract_attachments: None,
                },
            ],
        };

        let client_good = Mutex::new(Some(client_good));
        let client_for: ClientFor = std::sync::Arc::new(move |account: &str| {
            assert!(
                account != "boom",
                "client_for panicked on purpose for {account}"
            );
            client_good
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("client used twice"))
        });

        // `#[tokio::test]` defaults to a current-thread runtime, so the
        // spawned task's panic runs on this test's own thread — libtest's
        // normal per-test output capture hides its backtrace on success,
        // same as any other passing test, with no extra hook juggling
        // needed. `tokio::spawn` catches it as a `JoinError`, never
        // unwinding into this test itself.
        let err = run_sync_all(
            &config,
            project_root,
            false,
            false,
            true,
            &OutputFormat::Table,
            20,
            client_for,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("one or more accounts failed"));

        // The panicking account never blocked or aborted the working one.
        let manifest = std::fs::read_to_string(output_good.join("manifest.jsonl")).unwrap();
        assert_eq!(manifest.lines().count(), 1);
    }

    #[tokio::test]
    async fn run_sync_all_resolves_relative_output_dir_against_project_root() {
        let server = wiremock::MockServer::start().await;
        mount_token(&server).await;
        mount_profile(&server).await;
        mount_message_list(&server, &["m1"]).await;
        mount_raw_get(&server, "m1").await;

        let client = bootstrapped_client(&server);

        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path();
        let relative = PathBuf::from("emails/acct-a");

        let config = GmailSyncAllConfig {
            concurrency: None,
            accounts: vec![GmailSyncAccountEntry {
                account: "acct-a".to_string(),
                output_dir: relative.clone(),
                query: None,
                extract_attachments: None,
            }],
        };

        let client = Mutex::new(Some(client));
        let client_for: ClientFor = std::sync::Arc::new(move |_: &str| {
            client
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("client used twice"))
        });

        run_sync_all(
            &config,
            project_root,
            false,
            false,
            true,
            &OutputFormat::Table,
            20,
            client_for,
        )
        .await
        .unwrap();

        // `output_dir: relative` in the config must resolve against
        // `project_root`, not the process's actual CWD.
        let manifest =
            std::fs::read_to_string(project_root.join(&relative).join("manifest.jsonl")).unwrap();
        assert_eq!(manifest.lines().count(), 1);
    }

    #[tokio::test]
    async fn run_sync_all_writes_structured_output_for_a_non_table_format() {
        let server = wiremock::MockServer::start().await;
        mount_token(&server).await;
        mount_profile(&server).await;
        mount_message_list(&server, &["m1"]).await;
        mount_raw_get(&server, "m1").await;

        let client = bootstrapped_client(&server);

        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path();
        let output_dir = project_root.join("emails/acct-a");

        let config = GmailSyncAllConfig {
            concurrency: None,
            accounts: vec![GmailSyncAccountEntry {
                account: "acct-a".to_string(),
                output_dir: output_dir.clone(),
                query: None,
                extract_attachments: None,
            }],
        };

        let client = Mutex::new(Some(client));
        let client_for: ClientFor = std::sync::Arc::new(move |_: &str| {
            client
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("client used twice"))
        });

        // `-o json` (any non-`Table` format) takes `run_sync_all`'s other
        // rendering branch: a single `SyncAllReportOutput` via `output_as`
        // instead of the per-account `println!`s `-o table` uses.
        run_sync_all(
            &config,
            project_root,
            false,
            false,
            true,
            &OutputFormat::Json,
            20,
            client_for,
        )
        .await
        .unwrap();
    }

    // ── SyncAllReportOutput::write_jsonl ──────────────────────────────────

    #[test]
    fn sync_all_report_output_write_jsonl_emits_exactly_one_line() {
        let output = SyncAllReportOutput {
            accounts: vec![AccountSyncOutcome {
                account: "acct-a".to_string(),
                account_error: None,
                actions: Vec::new(),
                errors: Vec::new(),
                summary: SyncSummary::default(),
            }],
            combined_summary: SyncSummary::default(),
        };

        let mut buf = Vec::new();
        output.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();

        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("acct-a"));
    }
}
