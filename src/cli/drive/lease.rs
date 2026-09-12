//! CLI commands for `omni-dev drive lease` — the Drive write lease
//! ([ADR-0080](../../../docs/adrs/adr-0080.md)).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::lease::acquire::{
    self, AcquireOptions, AcquireResult, MAX_EXPIRY_MINUTES, MIN_EXPIRY_MINUTES,
};
use crate::drive::lease::authenticate::{self, AuthPolicy};
use crate::drive::lease::ledger::{self, LeaseBackup};
use crate::drive::lease::restore::{self, RestoreOptions, RestoreResult};

/// Default lease expiry when `--expiry-minutes` is not given (ADR-0080 §5).
const DEFAULT_EXPIRY_MINUTES: i64 = 30;

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
}

impl LeaseCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.action {
            LeaseAction::Acquire(cmd) => cmd.execute(client).await,
            LeaseAction::Restore(cmd) => cmd.execute(client).await,
        }
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

    /// Local directory byte backups are written under. Defaults to
    /// `<state dir>/omni-dev/drive-backups`.
    #[arg(long, value_name = "PATH")]
    pub backup_dir: Option<std::path::PathBuf>,

    /// Minutes the lease stays live once authorised. A write never
    /// extends this — a fresh window means a fresh `drive lease acquire`
    /// (ADR-0080 §5).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_EXPIRY_MINUTES, value_parser = parse_expiry_minutes)]
    pub expiry_minutes: i64,

    /// Require Touch ID specifically, failing outright rather than
    /// falling back to the account password (ADR-0080 §7). Needs Touch ID
    /// hardware; the default policy works on any Mac.
    #[arg(long)]
    pub biometrics_only: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AcquireCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let backup_dir = match self.backup_dir {
            Some(dir) => dir,
            None => default_backup_dir()?,
        };
        let ledger_path = ledger::ledger_path()?;
        let native_backup_folder_id =
            crate::cli::drive::helpers::active_account_lease_backup_folder_id()?;
        let opts = AcquireOptions {
            file_id: self.file_id,
            backup_dir,
            native_backup_folder_id,
            expiry: chrono::Duration::minutes(self.expiry_minutes),
            auth_policy: if self.biometrics_only {
                AuthPolicy::BiometricsOnly
            } else {
                AuthPolicy::DeviceOwner
            },
            ledger_path,
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

/// `<state dir>/omni-dev/drive-backups` — a sibling of the request log and
/// lease ledger, same posture.
fn default_backup_dir() -> Result<std::path::PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("could not resolve the state/data directory for the default backup directory")?;
    Ok(base.join("omni-dev").join("drive-backups"))
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

    /// Local directory the fresh lease's own byte backup is written under.
    /// Defaults to `<state dir>/omni-dev/drive-backups`.
    #[arg(long, value_name = "PATH")]
    pub backup_dir: Option<std::path::PathBuf>,

    /// Minutes the fresh lease stays live once authorised.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_EXPIRY_MINUTES, value_parser = parse_expiry_minutes)]
    pub expiry_minutes: i64,

    /// Require Touch ID specifically for the fresh lease, failing outright
    /// rather than falling back to the account password (ADR-0080 §7).
    #[arg(long)]
    pub biometrics_only: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl RestoreCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let backup_dir = match self.backup_dir {
            Some(dir) => dir,
            None => default_backup_dir()?,
        };
        let ledger_path = ledger::ledger_path()?;
        let native_backup_folder_id =
            crate::cli::drive::helpers::active_account_lease_backup_folder_id()?;
        let rules = crate::cli::drive::helpers::active_account_rules()?;
        let opts = RestoreOptions {
            token: self.token,
            backup_dir,
            native_backup_folder_id,
            expiry: chrono::Duration::minutes(self.expiry_minutes),
            auth_policy: if self.biometrics_only {
                AuthPolicy::BiometricsOnly
            } else {
                AuthPolicy::DeviceOwner
            },
            ledger_path,
        };
        let authenticator = authenticate::platform_authenticator();
        let result = restore::restore(client, &opts, authenticator.as_ref(), &rules).await;
        if output_as(&result, &self.output)? {
            return Ok(());
        }
        print_restore_result(&result);
        Ok(())
    }
}

fn print_result(result: &AcquireResult) {
    match result {
        AcquireResult::Acquired {
            token,
            expires_at,
            backup,
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
                backup_dir: None,
                expiry_minutes: DEFAULT_EXPIRY_MINUTES,
                biometrics_only: false,
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
            backup_dir: Some(root.path().join("backups")),
            expiry_minutes: 10,
            biometrics_only: true,
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
                backup_dir: None,
                expiry_minutes: DEFAULT_EXPIRY_MINUTES,
                biometrics_only: false,
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
        });
        ledger.save(&ledger_path).unwrap();

        let cmd = RestoreCommand {
            token: "backup-token".to_string(),
            backup_dir: None,
            expiry_minutes: DEFAULT_EXPIRY_MINUTES,
            biometrics_only: false,
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
            },
            RestoreResult::Restored {
                new_token: "tok-2".to_string(),
                expires_at: chrono::Utc::now(),
                backup: LeaseBackup::DriveCopy {
                    file_id: "copy-1".to_string(),
                },
            },
            RestoreResult::NoSuchBackupToken,
            RestoreResult::NoTypedRestorePath {
                backup_location: "copy-1".to_string(),
            },
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
        let dir = default_backup_dir().unwrap();
        assert!(
            dir.ends_with(std::path::Path::new("omni-dev").join("drive-backups")),
            "{}",
            dir.display()
        );
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
            },
            AcquireResult::Acquired {
                token: "tok-2".to_string(),
                expires_at: chrono::Utc::now(),
                backup: LeaseBackup::DriveCopy {
                    file_id: "copy-1".to_string(),
                },
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
                LeaseAction::Restore(_) => panic!("expected an Acquire command"),
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
        assert!(cmd.backup_dir.is_none());
        assert_eq!(cmd.expiry_minutes, DEFAULT_EXPIRY_MINUTES);
        assert!(!cmd.biometrics_only);
    }

    #[test]
    fn expiry_minutes_boundary_values_are_accepted() {
        assert_eq!(
            parse(&["acquire", "file1", "--expiry-minutes", "1"]).expiry_minutes,
            1
        );
        assert_eq!(
            parse(&["acquire", "file1", "--expiry-minutes", "1440"]).expiry_minutes,
            1440
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
        ]);
        assert_eq!(
            cmd.backup_dir,
            Some(std::path::PathBuf::from("/tmp/backups"))
        );
        assert_eq!(cmd.expiry_minutes, 10);
        assert!(cmd.biometrics_only);
    }
}
