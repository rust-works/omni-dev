//! CLI commands for `omni-dev drive lease` — the Drive write lease
//! ([ADR-0080](../../../docs/adrs/adr-0080.md)).

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::lease::acquire::{
    self, AcquireOptions, AcquireResult, MAX_EXPIRY_MINUTES, MIN_EXPIRY_MINUTES,
};
use crate::drive::lease::authenticate::{self, AuthPolicy};
use crate::drive::lease::ledger::{self, LeaseBackup};
use crate::drive::lease::prune::{self, PruneOptions};
use crate::drive::lease::restore::{self, RestoreOptions, RestoreResult};
use crate::drive::lease::settings as lease_settings;
use crate::drive::sheets::client::SheetsClient;
use crate::utils::settings::{Settings, SettingsEnv};

/// Validates `--expiry-minutes` before anything else runs — no network
/// call, no Touch ID prompt — so a bad value is a clean parse error instead
/// of either an instantly-dead lease (a non-positive value) or a panic
/// (`chrono::Duration::minutes` overflowing on an extreme one). The
/// accepted range itself ([`MIN_EXPIRY_MINUTES`]/[`MAX_EXPIRY_MINUTES`]) is
/// owned by the engine, not duplicated here, so the CLI parser and the
/// engine's own defense-in-depth check (issue #1664 review finding) can't
/// drift apart.
fn parse_expiry_minutes(s: &str) -> Result<i64, String> {
    let value: i64 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a valid integer"))?;
    if !(MIN_EXPIRY_MINUTES..=MAX_EXPIRY_MINUTES).contains(&value) {
        return Err(format!(
            "must be between {MIN_EXPIRY_MINUTES} and {MAX_EXPIRY_MINUTES} (24 hours), got {value}"
        ));
    }
    Ok(value)
}

/// The Drive write lease: acquire a Touch ID-authorised backup token before
/// a content-mutating write.
#[derive(Parser)]
pub struct LeaseCommand {
    #[command(subcommand)]
    action: LeaseAction,
}

#[derive(Subcommand)]
enum LeaseAction {
    /// Backs up a file and mints a lease token, prompting for device-owner
    /// authentication (Touch ID or the account password).
    Acquire(AcquireCommand),
    /// Restores a file from a backup lease's recorded content, minting a
    /// fresh lease of its own before writing.
    Restore(RestoreCommand),
    /// Bounds the ledger's and the backup directory/folder's growth by
    /// dropping expired rows together with the backups they point at
    /// (ADR-0080 Consequences fast-follow, #1678).
    Prune(PruneCommand),
}

impl LeaseCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.action {
            LeaseAction::Acquire(cmd) => cmd.execute(client).await,
            LeaseAction::Restore(cmd) => cmd.execute(client).await,
            LeaseAction::Prune(cmd) => cmd.execute(client).await,
        }
    }
}

/// The four global-policy flags shared verbatim by `acquire` and `restore`
/// (ADR-0080 §13, issue #1677) — flattened into both subcommands rather than
/// duplicated, since each also needs the identical resolution logic in
/// [`LeaseFlags::resolve`].
#[derive(Parser)]
pub struct LeaseFlags {
    /// Local directory byte backups are written under. Defaults to
    /// `OMNI_DEV_DRIVE_LEASE_BACKUP_DIR`, then `settings.json`'s
    /// `lease.backup_dir`, then `<state dir>/omni-dev/drive-backups`.
    #[arg(long, value_name = "PATH")]
    pub backup_dir: Option<std::path::PathBuf>,

    /// Minutes the lease stays live once authorised. A write never extends
    /// this — a fresh window means a fresh `drive lease acquire` (ADR-0080
    /// §5). Defaults to `OMNI_DEV_DRIVE_LEASE_EXPIRY_MINUTES`, then
    /// `settings.json`'s `lease.default_expiry_minutes`, then 30.
    #[arg(long, value_name = "N", value_parser = parse_expiry_minutes)]
    pub expiry_minutes: Option<i64>,

    /// Require Touch ID specifically, failing outright rather than falling
    /// back to the account password (ADR-0080 §7). Needs Touch ID hardware;
    /// the default policy works on any Mac. Also settable via
    /// `settings.json`'s `lease.biometrics_only` or
    /// `OMNI_DEV_DRIVE_LEASE_BIOMETRICS_ONLY`; any layer selecting it wins.
    #[arg(long)]
    pub biometrics_only: bool,

    /// Proceed even when no device-owner authenticator is available in this
    /// context — off-macOS, or a macOS process with no attached GUI session
    /// (ADR-0080 §8) — waiving the human-presence guarantee instead of
    /// refusing outright. Also settable via `settings.json`'s
    /// `lease.allow_headless` or `OMNI_DEV_DRIVE_LEASE_ALLOW_HEADLESS`; any
    /// layer opting in wins.
    #[arg(long)]
    pub allow_headless: bool,
}

/// What [`LeaseFlags`] resolved to, after layering the CLI flags over
/// `OMNI_DEV_DRIVE_LEASE_*`/`settings.json`/hard-coded defaults.
struct ResolvedLeaseFlags {
    backup_dir: std::path::PathBuf,
    expiry: chrono::Duration,
    auth_policy: AuthPolicy,
    allow_headless: bool,
}

impl LeaseFlags {
    fn resolve(self) -> Result<ResolvedLeaseFlags> {
        // One disk read/parse of settings.json, not two — `SettingsEnv::load()`
        // and `Settings::load_lease()` each independently re-read it;
        // `SettingsEnv::from_settings` was added for exactly this (issue
        // #1533), so both views come from the same parse (issue #1677 review
        // finding). A parse failure warns rather than silently falling back
        // (issue #1695) — `biometrics_only`'s default is the less-secure
        // direction, so a broken settings.json must not silently downgrade
        // it.
        let loaded = Settings::load().unwrap_or_else(|e| {
            tracing::warn!(
                "{e:#}; falling back to default settings for this invocation — any \
                 `lease.*` config in settings.json (backup_dir, default_expiry_minutes, \
                 biometrics_only, allow_headless) is being ignored"
            );
            Settings::default()
        });
        let lease = loaded.lease.clone();
        let profile = crate::utils::settings::active_profile_from(&crate::utils::env::SystemEnv);
        let env = SettingsEnv::from_settings(loaded, profile.as_deref());
        let backup_dir = lease_settings::resolve_backup_dir(self.backup_dir, &env, &lease)?;
        let expiry = chrono::Duration::minutes(lease_settings::resolve_expiry_minutes(
            self.expiry_minutes,
            &env,
            &lease,
        )?);
        let auth_policy = lease_settings::resolve_auth_policy(self.biometrics_only, &env, &lease);
        let allow_headless =
            lease_settings::resolve_allow_headless(self.allow_headless, &env, &lease);
        Ok(ResolvedLeaseFlags {
            backup_dir,
            expiry,
            auth_policy,
            allow_headless,
        })
    }
}

/// Backs up `FILE_ID`'s current content and mints a lease token
/// (ADR-0080 §2). Refuses a Google-native document (Docs/Sheets/Slides)
/// unless the active account has `lease_backup_folder_id` configured, in
/// which case it backs up via a lossless Drive-side `files.copy` into that
/// folder instead (ADR-0080 §3).
#[derive(Parser)]
pub struct AcquireCommand {
    /// Drive file id to lease (from `drive search`, or the `id` segment of
    /// a Drive URL).
    pub file_id: String,

    #[command(flatten)]
    pub flags: LeaseFlags,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AcquireCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let resolved = self.flags.resolve()?;
        let ledger_path = ledger::ledger_path()?;
        let native_backup_folder_id =
            crate::cli::drive::helpers::active_account_lease_backup_folder_id()?;
        let opts = AcquireOptions {
            file_id: self.file_id,
            backup_dir: resolved.backup_dir,
            native_backup_folder_id,
            expiry: resolved.expiry,
            auth_policy: resolved.auth_policy,
            ledger_path,
            allow_headless: resolved.allow_headless,
        };
        let authenticator = authenticate::platform_authenticator();
        let result = acquire::acquire(client, &opts, authenticator.as_ref()).await;
        if output_as(&result, &self.output)? {
            return Ok(());
        }
        print_result(&result);
        Ok(())
    }
}

/// Restores a file from the backup a lease recorded (ADR-0080 §10). `TOKEN`
/// is the *backup* lease — it locates the backup and authorises nothing
/// itself; restoring mints its own fresh lease, prompting for device-owner
/// authentication the same way `acquire` does.
#[derive(Parser)]
pub struct RestoreCommand {
    /// The backup lease's token (from `drive lease acquire`), expired or
    /// not — an expired-but-kept row is the expected common case.
    pub token: String,

    #[command(flatten)]
    pub flags: LeaseFlags,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl RestoreCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let resolved = self.flags.resolve()?;
        let ledger_path = ledger::ledger_path()?;
        let native_backup_folder_id =
            crate::cli::drive::helpers::active_account_lease_backup_folder_id()?;
        let rules = crate::cli::drive::helpers::active_account_rules()?;
        let opts = RestoreOptions {
            token: self.token,
            backup_dir: resolved.backup_dir,
            native_backup_folder_id,
            expiry: resolved.expiry,
            auth_policy: resolved.auth_policy,
            ledger_path,
            allow_headless: resolved.allow_headless,
        };
        let sheets = SheetsClient::from_drive_client(client)?;
        let authenticator = authenticate::platform_authenticator();
        let result = restore::restore(client, &sheets, &opts, authenticator.as_ref(), &rules).await;
        if output_as(&result, &self.output)? {
            return Ok(());
        }
        print_restore_result(&result);
        Ok(())
    }
}

/// Bounds the lease ledger's and the backup directory/folder's growth
/// (ADR-0080 Consequences fast-follow, #1678). Mirrors `omni-dev log
/// prune`'s shape: `--older-than`/`--max-size`, at least one required,
/// applied sequentially (age first, then size trims what's left),
/// `--dry-run` reports without mutating anything. Unlike `log prune`, there
/// is no `--audit` flag to refuse — this command's underlying artifacts
/// (the ledger, the backup directory/folder) never include `audit.jsonl` in
/// the first place.
///
/// No [`LeaseFlags`] here: prune touches no per-lease `backup_dir`/
/// `expiry_minutes`/Touch-ID policy — every row already carries its own
/// absolute backup path or Drive file id, and pruning never mints a new
/// lease.
#[derive(Parser)]
pub struct PruneCommand {
    /// Drop non-live rows whose expiry is older than this relative window
    /// (e.g. `7d`, `24h`, `2w`).
    #[arg(long, value_name = "DUR")]
    older_than: Option<String>,
    /// After `--older-than`, additionally drop the oldest-expiring
    /// survivors until their local backup bytes total at most this size
    /// (e.g. `10mb`, `512kb`, `1048576`). A `DriveCopy` backup counts as
    /// zero bytes here — it consumes no local disk.
    #[arg(long, value_name = "SIZE")]
    max_size: Option<String>,
    /// Report what would be removed without deleting/trashing any backup
    /// or modifying the ledger.
    #[arg(long)]
    dry_run: bool,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl PruneCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        if self.older_than.is_none() && self.max_size.is_none() {
            anyhow::bail!("nothing to prune: pass --older-than <DUR> and/or --max-size <SIZE>");
        }
        let older_than = match self.older_than.as_deref() {
            Some(s) => Some(
                crate::cli::log::parse_since(s)
                    .map_err(|e| anyhow::anyhow!("invalid --older-than: {e}"))?,
            ),
            None => None,
        };
        let max_size = match self.max_size.as_deref() {
            Some(s) => Some(
                crate::request_log::parse_size(s)
                    .map_err(|e| anyhow::anyhow!("invalid --max-size: {e}"))?,
            ),
            None => None,
        };

        let ledger_path = ledger::ledger_path()?;
        let opts = PruneOptions {
            older_than,
            max_size,
            dry_run: self.dry_run,
            ledger_path,
        };
        let outcome = prune::prune(client, &opts).await?;
        if output_as(&outcome, &self.output)? {
            return Ok(());
        }
        let (verb, backup_verb, byte_verb) = if self.dry_run {
            ("Would remove", "would trash", "would free")
        } else {
            ("Removed", "trashed", "freed")
        };
        println!(
            "{verb} {} lease(s); kept {} ({} {backup_verb} Drive backup(s), {} failure(s), {byte_verb} \
             {} bytes of local backups).",
            outcome.removed,
            outcome.kept,
            outcome.trashed_drive_copies,
            outcome.failed,
            outcome.bytes_freed,
        );
        Ok(())
    }
}

fn print_result(result: &AcquireResult) {
    match result {
        AcquireResult::Acquired {
            token,
            expires_at,
            backup,
            headless_waiver,
        } => {
            println!("{token}");
            // The backup path embeds the file's Drive name (`backup_name`
            // in `acquire.rs`), which is server-controlled — sanitize
            // before it reaches the terminal, the same as every other
            // server-supplied string this CLI prints (#1137).
            let backup_desc = match backup {
                LeaseBackup::Bytes { path, .. } => {
                    sanitize_for_terminal(&path.display().to_string())
                }
                LeaseBackup::DriveCopy { file_id } => {
                    format!("Drive copy {}", sanitize_for_terminal(file_id))
                }
            };
            eprintln!("Backed up to {backup_desc} (expires {expires_at})");
            if *headless_waiver {
                eprintln!(
                    "Warning: acquired under the headless opt-out (ADR-0080 §8) — no \
                     device-owner prompt was presented for this lease."
                );
            }
        }
        AcquireResult::AlreadyLeased { token, expires_at } => {
            println!("{token}");
            eprintln!(
                "A live lease already covers this file (expires {expires_at}) — reusing its \
                 token rather than minting a second one"
            );
        }
        AcquireResult::RefusedNativeDocument => {
            eprintln!(
                "Refused: this is a Google-native document (Doc/Sheet/Slide) and no backup \
                 folder is configured for this account — set `lease_backup_folder_id` in \
                 settings.json to enable leasing native documents"
            );
        }
        AcquireResult::Denied { detail } => eprintln!("Denied: {detail}"),
        AcquireResult::Unavailable { detail } => eprintln!("Unavailable: {detail}"),
        AcquireResult::Failed { detail } => eprintln!("Failed: {detail}"),
    }
}

fn print_restore_result(result: &RestoreResult) {
    match result {
        RestoreResult::Restored {
            new_token,
            expires_at,
            backup,
            headless_waiver,
        } => {
            println!("{new_token}");
            let backup_desc = match backup {
                LeaseBackup::Bytes { path, .. } => {
                    sanitize_for_terminal(&path.display().to_string())
                }
                LeaseBackup::DriveCopy { file_id } => {
                    format!("Drive copy {}", sanitize_for_terminal(file_id))
                }
            };
            eprintln!(
                "Restored. Backed up the pre-restore content to {backup_desc} (expires \
                 {expires_at})"
            );
            if *headless_waiver {
                eprintln!(
                    "Warning: the fresh lease was minted under the headless opt-out (ADR-0080 \
                     §8) — no device-owner prompt was presented for it."
                );
            }
        }
        RestoreResult::RestoredSheet {
            new_token,
            expires_at,
            backup,
            spreadsheet_id,
            sheet_id,
            sheet_title,
            headless_waiver,
        } => {
            println!("{new_token}");
            let backup_desc = match backup {
                // omni-dev: coverage ignore reason="a sheet restore's fresh backup is always a DriveCopy — restore.rs's own recheck refuses unless the target is still a spreadsheet, and acquire() only ever takes a Bytes backup of a non-native target; this arm exists solely for exhaustiveness over the shared LeaseBackup enum"
                LeaseBackup::Bytes { path, .. } => {
                    sanitize_for_terminal(&path.display().to_string())
                }
                // omni-dev: coverage end
                LeaseBackup::DriveCopy { file_id } => {
                    format!("Drive copy {}", sanitize_for_terminal(file_id))
                }
            };
            eprintln!(
                "Restored sheet '{}' (id {sheet_id}) back into spreadsheet {}. Backed up the \
                 pre-restore content to {backup_desc} (expires {expires_at})",
                sanitize_for_terminal(sheet_title),
                sanitize_for_terminal(spreadsheet_id)
            );
            if *headless_waiver {
                eprintln!(
                    "Warning: the fresh lease was minted under the headless opt-out (ADR-0080 \
                     §8) — no device-owner prompt was presented for it."
                );
            }
        }
        RestoreResult::SheetAlreadyRestored {
            spreadsheet_id,
            sheet_id,
            sheet_title,
            restored_at,
            live_lease,
        } => {
            let when = restored_at.map_or_else(String::new, |at| format!(" on {at}"));
            eprintln!(
                "Refused: this backup's deleted sheet was already restored{when} into \
                 spreadsheet {} as '{}' (id {sheet_id}), which is still there — restoring \
                 again would only add a second copy. Delete that sheet first if you do want \
                 another one. No fresh lease was minted, no Touch ID was spent.",
                sanitize_for_terminal(spreadsheet_id),
                sanitize_for_terminal(sheet_title)
            );
            if let Some(lease) = live_lease {
                println!("{}", lease.token);
                eprintln!(
                    "A lease is still live for this file (expires {}, not necessarily minted \
                     by that earlier restore) — present it to `--lease` rather than spending \
                     another prompt",
                    lease.expires_at
                );
            }
        }
        RestoreResult::NoSuchBackupToken => {
            eprintln!(
                "Refused: no lease in this ledger was ever acquired with that token — check it \
                 was copied correctly from `drive lease acquire`'s own output"
            );
        }
        RestoreResult::NoTypedRestorePath { backup_location } => {
            eprintln!(
                "No typed restore path exists for this backup yet — it is a Drive copy at {} \
                 you can restore from by hand in the Drive UI",
                sanitize_for_terminal(backup_location)
            );
        }
        RestoreResult::BackupTooLargeForSimpleUpload { size } => {
            eprintln!(
                "Refused: this backup is {size} bytes, over Drive's 5 MB simple-upload limit — \
                 restoring it is not supported yet (no fresh lease was minted, no Touch ID was \
                 spent)"
            );
        }
        RestoreResult::RefusedNoVisibleParents => {
            eprintln!(
                "Refused: this file has no parent folder visible to this account, so no folder \
                 rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<file \
                 id>\", \"allow\": [\"edit\"]}} to write_permissions.rules."
            );
        }
        RestoreResult::Blocked { decided_by } => {
            eprintln!("Blocked");
            match decided_by {
                Some(rule) => eprintln!(
                    "  refused by rule on {} {}{}",
                    rule.kind_label(),
                    sanitize_for_terminal(rule.id()),
                    rule.depth_suffix()
                ),
                None => eprintln!("  refused by default policy (no matching rule)"),
            }
        }
        RestoreResult::AlreadyLeased { token, expires_at } => {
            println!("{token}");
            eprintln!(
                "A live lease already covers this file (expires {expires_at}) — reusing its \
                 token rather than minting a second one; re-run once it expires to restore"
            );
        }
        RestoreResult::RefusedNativeDocument => {
            eprintln!(
                "Refused: this is a Google-native document and no backup folder is configured \
                 for this account — set `lease_backup_folder_id` in settings.json"
            );
        }
        RestoreResult::Denied { detail } => eprintln!("Denied: {detail}"),
        RestoreResult::Unavailable { detail } => eprintln!("Unavailable: {detail}"),
        RestoreResult::Failed { detail } => eprintln!("Failed: {detail}"),
        RestoreResult::FreshLeaseButWriteFailed {
            token,
            expires_at,
            detail,
        } => {
            // The token is printed even though the write itself failed —
            // it is real and live (Touch ID was answered, a backup of the
            // current state was taken, a ledger row was written), so it
            // must not be surfaced nowhere the caller could ever find it
            // again.
            println!("{token}");
            eprintln!(
                "Failed: {detail}\nA fresh lease was minted before the failure and is still \
                 live (expires {expires_at}) — present it to `--lease`, or re-run `drive lease \
                 restore` with it, rather than spending another prompt"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
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

        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    /// A native-document target is refused before `acquire` ever calls the
    /// authenticator (see `acquire::tests::native_document_is_refused_before_authenticating`),
    /// so this exercises `LeaseCommand::execute`/`AcquireCommand::execute`
    /// end to end without risking a real Touch ID prompt in a test process.
    async fn native_document_server() -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/vnd.google-apps.document"
                })),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn lease_command_dispatches_acquire_with_a_default_backup_dir() {
        // Isolated so `active_account_lease_backup_folder_id` (which
        // `execute` now calls) reads an empty, unconfigured account rather
        // than this machine's real settings.json — the target here is a
        // native document with no backup folder configured, so it must
        // resolve `RefusedNativeDocument` deterministically regardless of
        // what Drive accounts happen to be configured locally.
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = native_document_server().await;
        let client = client_with_bootstrapped_token(&server).await;
        let cmd = LeaseCommand {
            action: LeaseAction::Acquire(AcquireCommand {
                file_id: "f1".to_string(),
                flags: LeaseFlags {
                    backup_dir: None,
                    expiry_minutes: None,
                    biometrics_only: false,
                    allow_headless: false,
                },
                output: OutputFormat::Table,
            }),
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn acquire_command_honours_an_explicit_backup_dir_and_biometrics_only() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = native_document_server().await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let cmd = AcquireCommand {
            file_id: "f1".to_string(),
            flags: LeaseFlags {
                backup_dir: Some(root.path().join("backups")),
                expiry_minutes: Some(10),
                biometrics_only: true,
                allow_headless: false,
            },
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn lease_command_dispatches_restore_with_no_such_backup_token() {
        // Isolated the same way the acquire dispatch test is: an
        // unconfigured account plus a token this fresh ledger has never
        // recorded resolves `NoSuchBackupToken` deterministically, with no
        // Drive call needed at all — cheap enough to double as "the
        // subcommand routes to `RestoreCommand::execute`".
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let cmd = LeaseCommand {
            action: LeaseAction::Restore(RestoreCommand {
                token: "no-such-token".to_string(),
                flags: LeaseFlags {
                    backup_dir: None,
                    expiry_minutes: None,
                    biometrics_only: false,
                    allow_headless: false,
                },
                output: OutputFormat::Table,
            }),
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn restore_command_execute_reaches_the_gate_and_is_blocked() {
        // Deliberately stops at the folder-permission gate, *before* the
        // internal fresh-`acquire` step would ever call
        // `authenticate::platform_authenticator()` — unlike every test in
        // `restore.rs` itself (which injects a fake `Authenticator`),
        // `RestoreCommand::execute` always resolves the *real* one, exactly
        // like `AcquireCommand::execute` already does. Actually reaching it
        // here would make this test depend on this machine's real
        // LocalAuthentication state (Touch ID enrolled and interactively
        // answered, or not) rather than being hermetic — precisely why
        // `acquire_command_honours_an_explicit_backup_dir_and_biometrics_only`
        // above uses a fixture that resolves `RefusedNativeDocument` for
        // the identical reason. The full authorized-restore path is
        // already covered, with an injected fake, by `restore.rs`'s own
        // `a_successful_restore_re_uploads_the_backup_and_mints_a_fresh_lease`.
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        // No write_permissions configured for this unconfigured account, so
        // the gate's bare default policy denies — reached only after this
        // command resolves the ledger path, loads the backup token's row,
        // and fetches the target + its parent, exercising the whole CLI
        // wiring short of authentication itself.

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                    "parents": ["parent-1"], "version": "1",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1",
                    "mimeType": crate::drive::types::GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        // Deliberately no download/PATCH mock: reaching either (which would
        // require authenticating first) fails the test.

        // Seed a backup lease directly into the ledger `RestoreCommand`
        // will itself resolve, under the `HOME` the guard already
        // redirected.
        let ledger_path = crate::drive::lease::ledger::ledger_path().unwrap();
        let backup_path = dir.path().join("original.bin");
        std::fs::write(&backup_path, b"original").unwrap();
        let sha256 = crate::cli::drive::read::to_hex_string(&{
            use sha2::Digest as _;
            sha2::Sha256::digest(b"original")
        });
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: "backup-token".to_string(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: backup_path,
                sha256,
                size: b"original".len() as u64,
            },
            acquired_at: chrono::Utc::now() - chrono::Duration::hours(2),
            expires_at: chrono::Utc::now() - chrono::Duration::hours(1),
            released_at: None,
            restored_at: None,
            restored_sheet_id: None,
        });
        ledger.save(&ledger_path).unwrap();

        // An explicit `--backup-dir` and `--biometrics-only`, exercising
        // `RestoreCommand::execute`'s own option-resolution branches — the
        // gate still blocks first, so neither ever reaches use.
        let cmd = RestoreCommand {
            token: "backup-token".to_string(),
            flags: LeaseFlags {
                backup_dir: Some(dir.path().join("fresh-backups")),
                expiry_minutes: None,
                biometrics_only: true,
                allow_headless: false,
            },
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();

        let reloaded = crate::drive::lease::ledger::LeaseLedger::load(&ledger_path).unwrap();
        assert!(
            reloaded.get("backup-token").unwrap().restored_at.is_none(),
            "a blocked restore must never mark the backup lease as restored-from"
        );
    }

    #[test]
    fn print_restore_result_does_not_panic_for_any_variant() {
        for result in [
            RestoreResult::Restored {
                new_token: "tok-1".to_string(),
                expires_at: chrono::Utc::now(),
                backup: LeaseBackup::Bytes {
                    path: std::path::PathBuf::from("/tmp/backup"),
                    sha256: "deadbeef".to_string(),
                    size: 0,
                },
                headless_waiver: false,
            },
            RestoreResult::Restored {
                new_token: "tok-2".to_string(),
                expires_at: chrono::Utc::now(),
                backup: LeaseBackup::DriveCopy {
                    file_id: "copy-1".to_string(),
                },
                headless_waiver: true,
            },
            RestoreResult::RestoredSheet {
                new_token: "tok-5".to_string(),
                expires_at: chrono::Utc::now(),
                backup: LeaseBackup::DriveCopy {
                    file_id: "copy-2".to_string(),
                },
                spreadsheet_id: "sheet-1".to_string(),
                sheet_id: 999,
                sheet_title: "Deleted".to_string(),
                headless_waiver: false,
            },
            // Both halves of the #1689 refusal: with a live lease to name,
            // and without one (the common case, once it has expired).
            RestoreResult::SheetAlreadyRestored {
                spreadsheet_id: "sheet-1".to_string(),
                sheet_id: 999,
                sheet_title: "Deleted".to_string(),
                restored_at: Some(chrono::Utc::now()),
                live_lease: Some(restore::LiveLease {
                    token: "tok-6".to_string(),
                    expires_at: chrono::Utc::now(),
                }),
            },
            RestoreResult::SheetAlreadyRestored {
                spreadsheet_id: "sheet-1".to_string(),
                sheet_id: 999,
                sheet_title: "Deleted".to_string(),
                restored_at: None,
                live_lease: None,
            },
            RestoreResult::NoSuchBackupToken,
            RestoreResult::NoTypedRestorePath {
                backup_location: "copy-1".to_string(),
            },
            RestoreResult::BackupTooLargeForSimpleUpload { size: 10_000_000 },
            RestoreResult::RefusedNoVisibleParents,
            RestoreResult::Blocked { decided_by: None },
            RestoreResult::Blocked {
                decided_by: Some(crate::drive::write_gate::DecidingRule::Folder {
                    folder_id: "parent-1".to_string(),
                    depth: 0,
                }),
            },
            RestoreResult::AlreadyLeased {
                token: "tok-3".to_string(),
                expires_at: chrono::Utc::now(),
            },
            RestoreResult::RefusedNativeDocument,
            RestoreResult::Denied {
                detail: "no".to_string(),
            },
            RestoreResult::Unavailable {
                detail: "no authenticator".to_string(),
            },
            RestoreResult::Failed {
                detail: "boom".to_string(),
            },
            RestoreResult::FreshLeaseButWriteFailed {
                token: "tok-4".to_string(),
                expires_at: chrono::Utc::now(),
                detail: "boom".to_string(),
            },
        ] {
            print_restore_result(&result);
        }
    }

    #[test]
    fn default_backup_dir_ends_with_the_expected_suffix() {
        // `LeaseFlags::resolve` delegates the whole chain to
        // `lease_settings::resolve_backup_dir`, whose own precedence tiers
        // are covered in `src/drive/lease/settings.rs` — this just confirms
        // the hard-coded bottom of the chain is still what this CLI's docs
        // promise.
        let dir = lease_settings::resolve_backup_dir(
            None,
            &crate::test_support::env::MapEnv::new(),
            &crate::utils::settings::LeaseSettings::default(),
        )
        .unwrap();
        assert!(
            dir.ends_with(std::path::Path::new("omni-dev").join("drive-backups")),
            "{}",
            dir.display()
        );
    }

    /// Thread-scoped log buffer, mirroring the `CaptureWriter`/`capture_info`
    /// pattern in `src/gmail/chrome_profile.rs`.
    #[derive(Clone, Default)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `f` under a thread-local WARN-level subscriber and returns
    /// everything it logged. `f` must be fully synchronous on this thread.
    fn capture_warn(f: impl FnOnce()) -> String {
        let writer = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8_lossy(&writer.0.lock().unwrap()).into_owned();
        logs
    }

    #[test]
    fn resolve_warns_and_falls_back_when_settings_json_fails_to_parse() {
        // A missing settings.json resolves to defaults with no warning
        // (the ordinary case, covered implicitly by every other test in
        // this module) — this covers the "file exists but doesn't parse"
        // case (issue #1695), which must warn rather than silently drop
        // `lease.biometrics_only`/`backup_dir`/`default_expiry_minutes`.
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_dir = dir.path().join(".omni-dev");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(settings_dir.join("settings.json"), "{not valid json").unwrap();

        let flags = LeaseFlags {
            backup_dir: None,
            expiry_minutes: None,
            biometrics_only: false,
            allow_headless: false,
        };
        let logs = capture_warn(|| {
            let resolved = flags.resolve().unwrap();
            assert_eq!(resolved.auth_policy, AuthPolicy::DeviceOwner);
        });
        assert!(logs.contains("settings.json"), "{logs}");
    }

    #[test]
    fn print_result_does_not_panic_for_any_variant() {
        for result in [
            AcquireResult::Acquired {
                token: "tok-1".to_string(),
                expires_at: chrono::Utc::now(),
                backup: LeaseBackup::Bytes {
                    path: std::path::PathBuf::from("/tmp/backup"),
                    sha256: "deadbeef".to_string(),
                    size: 0,
                },
                headless_waiver: false,
            },
            AcquireResult::Acquired {
                token: "tok-2".to_string(),
                expires_at: chrono::Utc::now(),
                backup: LeaseBackup::DriveCopy {
                    file_id: "copy-1".to_string(),
                },
                headless_waiver: true,
            },
            AcquireResult::AlreadyLeased {
                token: "tok-3".to_string(),
                expires_at: chrono::Utc::now(),
            },
            AcquireResult::RefusedNativeDocument,
            AcquireResult::Denied {
                detail: "no".to_string(),
            },
            AcquireResult::Unavailable {
                detail: "no authenticator".to_string(),
            },
            AcquireResult::Failed {
                detail: "boom".to_string(),
            },
        ] {
            print_result(&result);
        }
    }

    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: Wrapped,
    }

    #[derive(Subcommand)]
    enum Wrapped {
        Lease(LeaseCommand),
    }

    fn parse(args: &[&str]) -> AcquireCommand {
        let mut full = vec!["omni-dev", "lease"];
        full.extend_from_slice(args);
        match Wrapper::try_parse_from(full).unwrap().cmd {
            Wrapped::Lease(cmd) => match cmd.action {
                LeaseAction::Acquire(acquire) => acquire,
                // omni-dev: coverage ignore reason="guards this test helper against misuse; every call site below passes an acquire subcommand"
                LeaseAction::Restore(_) | LeaseAction::Prune(_) => {
                    panic!("expected an Acquire command")
                } // omni-dev: coverage end
            },
        }
    }

    fn parse_prune(args: &[&str]) -> PruneCommand {
        let mut full = vec!["omni-dev", "lease"];
        full.extend_from_slice(args);
        match Wrapper::try_parse_from(full).unwrap().cmd {
            Wrapped::Lease(cmd) => match cmd.action {
                LeaseAction::Prune(prune) => prune,
                // omni-dev: coverage ignore reason="guards this test helper against misuse; every call site below passes a prune subcommand"
                LeaseAction::Acquire(_) | LeaseAction::Restore(_) => {
                    panic!("expected a Prune command")
                } // omni-dev: coverage end
            },
        }
    }

    /// Like [`parse`], but for a value expected to fail `clap` validation.
    fn parse_err(args: &[&str]) -> String {
        let mut full = vec!["omni-dev", "lease"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full)
            .err()
            .expect("expected a parse error")
            .to_string()
    }

    #[test]
    fn defaults_are_sane() {
        let cmd = parse(&["acquire", "file1"]);
        assert_eq!(cmd.file_id, "file1");
        assert!(cmd.flags.backup_dir.is_none());
        assert!(cmd.flags.expiry_minutes.is_none());
        assert!(!cmd.flags.biometrics_only);
        assert!(!cmd.flags.allow_headless);
    }

    #[test]
    fn expiry_minutes_boundary_values_are_accepted() {
        assert_eq!(
            parse(&["acquire", "file1", "--expiry-minutes", "1"])
                .flags
                .expiry_minutes,
            Some(1)
        );
        assert_eq!(
            parse(&["acquire", "file1", "--expiry-minutes", "1440"])
                .flags
                .expiry_minutes,
            Some(1440)
        );
    }

    #[test]
    fn expiry_minutes_zero_is_rejected() {
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "0"]);
        assert!(err.contains("must be between 1 and 1440"), "{err}");
    }

    #[test]
    fn expiry_minutes_negative_is_rejected() {
        // clap treats a leading `-` as looking like a flag before our own
        // `value_parser` ever runs (`-- -5` is the escape hatch) — still a
        // clean rejection, just with clap's own message rather than ours.
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "-5"]);
        assert!(err.contains("unexpected argument"), "{err}");
    }

    #[test]
    fn expiry_minutes_past_the_cap_is_rejected() {
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "1441"]);
        assert!(err.contains("must be between 1 and 1440"), "{err}");
    }

    #[test]
    fn expiry_minutes_over_the_cap_but_still_a_valid_i64_is_rejected_cleanly() {
        // Large enough to be a plausible typo, small enough to still parse
        // as `i64` — exercises the range check specifically, as opposed to
        // the integer-parse failure below.
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "999999999999999999"]);
        assert!(err.contains("must be between 1 and 1440"), "{err}");
    }

    #[test]
    fn expiry_minutes_past_i64_range_is_a_clean_parse_error_not_a_panic() {
        // Would overflow `chrono::Duration::minutes` if it ever reached
        // that call — must be rejected as a plain integer-parse failure
        // instead, well before the range check.
        let err = parse_err(&[
            "acquire",
            "file1",
            "--expiry-minutes",
            "99999999999999999999",
        ]);
        assert!(err.contains("not a valid integer"), "{err}");
    }

    #[test]
    fn flags_parse() {
        let cmd = parse(&[
            "acquire",
            "file1",
            "--backup-dir",
            "/tmp/backups",
            "--expiry-minutes",
            "10",
            "--biometrics-only",
            "--allow-headless",
        ]);
        assert_eq!(
            cmd.flags.backup_dir,
            Some(std::path::PathBuf::from("/tmp/backups"))
        );
        assert_eq!(cmd.flags.expiry_minutes, Some(10));
        assert!(cmd.flags.biometrics_only);
        assert!(cmd.flags.allow_headless);
    }

    // ── prune ────────────────────────────────────────────────────────

    #[test]
    fn prune_flags_parse() {
        let cmd = parse_prune(&[
            "prune",
            "--older-than",
            "7d",
            "--max-size",
            "10mb",
            "--dry-run",
        ]);
        assert_eq!(cmd.older_than.as_deref(), Some("7d"));
        assert_eq!(cmd.max_size.as_deref(), Some("10mb"));
        assert!(cmd.dry_run);
    }

    #[tokio::test]
    async fn prune_requires_at_least_one_bound() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let cmd = parse_prune(&["prune"]);
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("nothing to prune"), "{err}");
    }

    #[tokio::test]
    async fn prune_rejects_an_invalid_older_than() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let cmd = parse_prune(&["prune", "--older-than", "not-a-duration"]);
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("invalid --older-than"), "{err}");
    }

    #[tokio::test]
    async fn prune_command_removes_an_expired_lease_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let ledger_path = crate::drive::lease::ledger::ledger_path().unwrap();
        let backup_path = dir.path().join("old-backup.bin");
        std::fs::write(&backup_path, b"stale").unwrap();
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: "old-token".to_string(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: backup_path.clone(),
                sha256: "deadbeef".to_string(),
                size: 5,
            },
            acquired_at: chrono::Utc::now() - chrono::Duration::days(10),
            expires_at: chrono::Utc::now() - chrono::Duration::days(9),
            released_at: None,
            restored_at: None,
            restored_sheet_id: None,
        });
        ledger.save(&ledger_path).unwrap();

        let cmd = PruneCommand {
            older_than: Some("1d".to_string()),
            max_size: None,
            dry_run: false,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();

        assert!(!backup_path.exists());
        let reloaded = crate::drive::lease::ledger::LeaseLedger::load(&ledger_path).unwrap();
        assert!(reloaded.get("old-token").is_none());
    }

    #[tokio::test]
    async fn prune_command_max_size_only_dispatches_through_lease_command_and_prints_a_table_summary(
    ) {
        // `--max-size` alone (no `--older-than`) through the top-level
        // `LeaseCommand::execute` dispatch, with the default `Table`
        // output — covers the `older_than: None` branch, the `--max-size`
        // parse branch, and the human-readable non-dry-run summary, none
        // of which the other prune tests (which always pass `--older-than`
        // and always request JSON output) reach.
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let ledger_path = crate::drive::lease::ledger::ledger_path().unwrap();
        let old_backup = dir.path().join("old-backup.bin");
        let new_backup = dir.path().join("new-backup.bin");
        std::fs::write(&old_backup, vec![0u8; 100]).unwrap();
        std::fs::write(&new_backup, vec![0u8; 100]).unwrap();
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: "old-token".to_string(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: old_backup.clone(),
                sha256: "deadbeef".to_string(),
                size: 100,
            },
            acquired_at: chrono::Utc::now() - chrono::Duration::hours(3),
            expires_at: chrono::Utc::now() - chrono::Duration::hours(2),
            released_at: None,
            restored_at: None,
            restored_sheet_id: None,
        });
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: "new-token".to_string(),
            file_id: "file-2".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: new_backup.clone(),
                sha256: "deadbeef".to_string(),
                size: 100,
            },
            acquired_at: chrono::Utc::now() - chrono::Duration::hours(2),
            expires_at: chrono::Utc::now() - chrono::Duration::hours(1),
            released_at: None,
            restored_at: None,
            restored_sheet_id: None,
        });
        ledger.save(&ledger_path).unwrap();

        let lease_cmd = LeaseCommand {
            action: LeaseAction::Prune(PruneCommand {
                older_than: None,
                max_size: Some("150".to_string()),
                dry_run: false,
                output: OutputFormat::Table,
            }),
        };
        lease_cmd.execute(&client).await.unwrap();

        assert!(!old_backup.exists());
        assert!(new_backup.exists());
        let reloaded = crate::drive::lease::ledger::LeaseLedger::load(&ledger_path).unwrap();
        assert!(reloaded.get("old-token").is_none());
        assert!(reloaded.get("new-token").is_some());
    }

    #[tokio::test]
    async fn prune_command_dry_run_prints_a_would_remove_table_summary() {
        // The dry-run half of the human-readable summary's verb/backup-verb
        // tuple — the non-dry-run half is covered above.
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let ledger_path = crate::drive::lease::ledger::ledger_path().unwrap();
        let backup_path = dir.path().join("old-backup.bin");
        std::fs::write(&backup_path, b"stale").unwrap();
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: "old-token".to_string(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: backup_path.clone(),
                sha256: "deadbeef".to_string(),
                size: 5,
            },
            acquired_at: chrono::Utc::now() - chrono::Duration::days(10),
            expires_at: chrono::Utc::now() - chrono::Duration::days(9),
            released_at: None,
            restored_at: None,
            restored_sheet_id: None,
        });
        ledger.save(&ledger_path).unwrap();

        let cmd = PruneCommand {
            older_than: Some("1d".to_string()),
            max_size: None,
            dry_run: true,
            output: OutputFormat::Table,
        };
        cmd.execute(&client).await.unwrap();

        assert!(backup_path.exists(), "dry-run must not delete the backup");
        let reloaded = crate::drive::lease::ledger::LeaseLedger::load(&ledger_path).unwrap();
        assert!(reloaded.get("old-token").is_some());
    }
}
