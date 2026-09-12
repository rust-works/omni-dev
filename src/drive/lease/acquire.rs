//! `drive lease acquire` — the engine behind
//! [ADR-0080](../../../docs/adrs/adr-0080.md) §2: authenticate, then back
//! up, then record, then mint. Binary files back up as bytes to local disk;
//! a Google-native document (Sheet/Doc/Slide) backs up as a lossless
//! Drive-side `files.copy` into the account's configured backup folder
//! (§3's fidelity split) — refused outright, before authenticating at all,
//! when no backup folder is configured for this account.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cli::drive::format::JsonlSerialize;
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::authenticate::{AuthOutcome, AuthPolicy, Authenticator};
use crate::drive::lease::ledger::{LeaseBackup, LeaseLedger, LeaseRecord, LedgerLock};

/// Per-call options for `drive lease acquire`.
#[derive(Debug, Clone)]
pub struct AcquireOptions {
    /// The file id to lease.
    pub file_id: String,
    /// Local directory byte backups are written under. Unused for a
    /// native-document target.
    pub backup_dir: PathBuf,
    /// Destination folder for a native document's Drive-side backup copy
    /// (ADR-0080 §3/§13) — the account's configured
    /// `lease_backup_folder_id`. `None` means a native-document target is
    /// refused outright; unused for a binary target.
    pub native_backup_folder_id: Option<String>,
    /// How long the lease stays live from the moment it is authorised.
    pub expiry: ChronoDuration,
    /// Which authentication policy to present (ADR-0080 §7).
    pub auth_policy: AuthPolicy,
    /// Path to the lease ledger. Production callers pass
    /// [`crate::drive::lease::ledger::ledger_path`]'s own result; tests
    /// pass a path under a `tempdir` so a test run never touches the real
    /// ledger.
    pub ledger_path: PathBuf,
}

/// What happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum AcquireResult {
    /// Authenticated, backed up, and recorded. Present the token to a
    /// gated write via `--lease`.
    Acquired {
        /// The opaque lease token.
        token: String,
        /// When this lease stops authorising writes.
        expires_at: DateTime<Utc>,
        /// Where the backup landed.
        backup: LeaseBackup,
    },
    /// The target is a Google-native document and no backup folder is
    /// configured for this account (`lease_backup_folder_id`) — nowhere to
    /// put the required Drive-side copy.
    RefusedNativeDocument,
    /// A human answered the prompt and refused, or it timed out.
    Denied {
        /// The platform's own message.
        detail: String,
    },
    /// No authenticator is available in this context (ADR-0080 §8) — no
    /// backup was taken and no ledger row was written.
    Unavailable {
        /// Why no authenticator is available.
        detail: String,
    },
    /// An API, filesystem, or ledger error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl JsonlSerialize for AcquireResult {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        crate::cli::drive::format::write_scalar_jsonl(self, out)
    }
}

/// Runs the four-step sequence ADR-0080 §2 specifies, in order, aborting at
/// the first failure with nothing further attempted, then records the
/// attempt to the audit sink regardless of outcome (ADR-0080 §11).
///
/// The audit record here is best-effort, not write-ahead/fail-closed the
/// way a leased *write*'s is: `drive lease acquire` mutates no Drive
/// content — it takes a backup and writes a ledger row, both already
/// durable by the time this function is about to return — so there is no
/// "mutating API call" for a write-ahead record to precede. A logging
/// failure here is warned, not surfaced as a failed acquisition: the
/// consent, the backup and the ledger row already happened, and turning a
/// genuine success into a reported failure because a follow-up log write
/// failed would make the tool's own output less trustworthy, not more.
pub async fn acquire(
    client: &DriveClient,
    opts: &AcquireOptions,
    authenticator: &dyn Authenticator,
) -> AcquireResult {
    let result = acquire_inner(client, opts, authenticator).await;
    record_attempt(opts, &result);
    result
}

async fn acquire_inner(
    client: &DriveClient,
    opts: &AcquireOptions,
    authenticator: &dyn Authenticator,
) -> AcquireResult {
    let files_api = FilesApi::new(client);
    let target = match files_api.get_metadata(&opts.file_id).await {
        Ok(target) => target,
        Err(err) => {
            return AcquireResult::Failed {
                detail: err.to_string(),
            }
        }
    };

    let is_native = target.is_google_native();
    if is_native && opts.native_backup_folder_id.is_none() {
        return AcquireResult::RefusedNativeDocument;
    }

    if target.version.is_none() {
        return AcquireResult::Failed {
            detail: "Drive did not return a `version` for this file; refusing to lease it \
                     without a staleness check"
                .to_string(),
        };
    }

    // 1. Authenticate — consent gates the action, not merely possession of
    // the resulting token (ADR-0080 §2). Nothing below runs on refusal.
    // `target.name` is Drive-controlled (renamable by anyone with edit
    // access to the file) and is shown verbatim inside the OS consent
    // prompt, so it is sanitized the same way any other server-supplied
    // string reaching a terminal or prompt is elsewhere in this CLI —
    // stripping control characters and bidi-override code points closes
    // off a spoofed/deceptive file name misleading what the operator is
    // authorising.
    let reason = format!(
        "back up and lease-write '{}'",
        sanitize_for_terminal(&target.name)
    );
    // `authenticate` blocks synchronously on the human's answer, up to
    // `PROMPT_TIMEOUT` (120s) — `block_in_place` hands this worker
    // thread's other queued tasks off to the runtime's other workers for
    // the duration, so a single Touch ID prompt cannot stall unrelated
    // concurrent work on a shared multi-thread runtime (the daemon and
    // the MCP server both run one). `acquire` itself stays a plain `async
    // fn` — `block_in_place` runs the closure on the *current* thread, so
    // it needs no `'static`/`Send` bound on `authenticator`, unlike
    // `spawn_blocking`.
    let auth_outcome =
        tokio::task::block_in_place(|| authenticator.authenticate(&reason, opts.auth_policy));
    match auth_outcome {
        AuthOutcome::Authorized => {}
        AuthOutcome::Denied(detail) => return AcquireResult::Denied { detail },
        AuthOutcome::Unavailable(detail) => return AcquireResult::Unavailable { detail },
    }

    // 2. Backup — bytes for a binary file, a Drive-side copy for a native
    // document (ADR-0080 §3). `native_backup_folder_id` is guaranteed
    // `Some` here whenever `is_native`, by the refusal above.
    let backup = if is_native {
        let folder_id = opts.native_backup_folder_id.as_deref().unwrap_or_default();
        match native_backup(&files_api, &opts.file_id, folder_id, &target.name).await {
            Ok(backup) => backup,
            Err(err) => {
                return AcquireResult::Failed {
                    detail: err.to_string(),
                }
            }
        }
    } else {
        match byte_backup(&files_api, &opts.file_id, &opts.backup_dir, &target.name).await {
            Ok(backup) => backup,
            Err(err) => {
                return AcquireResult::Failed {
                    detail: err.to_string(),
                }
            }
        }
    };

    // 3. Ledger record. `version`/`modified_time` are re-fetched here
    // rather than reused from the `target` metadata read at the very top —
    // that read happened before the (up to 120s) Touch ID prompt and
    // before the backup itself, so the file could have moved in the
    // meantime. Recording that earlier, now-possibly-stale version would
    // break the token's own invariant of being "bound to a specific backup
    // and a specific Drive version" (ADR-0080 §2/§4): the backup reflects
    // whatever the file was at backup time, so the recorded version must
    // too, or the very next write under this lease could spuriously refuse
    // as stale (or, worse, pass a staleness check against a version that
    // doesn't match what was actually backed up).
    let post_backup = match files_api.get_metadata(&opts.file_id).await {
        Ok(post_backup) => post_backup,
        Err(err) => {
            return AcquireResult::Failed {
                detail: err.to_string(),
            }
        }
    };
    let Some(version) = post_backup.version else {
        return AcquireResult::Failed {
            detail: "Drive did not return a `version` for this file after the backup; \
                     refusing to record a lease without a staleness check"
                .to_string(),
        };
    };
    let token = crate::request_log::new_id();
    let now = Utc::now();
    let expires_at = now + opts.expiry;
    let record = LeaseRecord {
        token: token.clone(),
        file_id: opts.file_id.clone(),
        version,
        modified_time: post_backup.modified_time,
        backup: backup.clone(),
        acquired_at: now,
        expires_at,
        released_at: None,
    };
    if let Err(err) = insert_record(record, &opts.ledger_path) {
        return AcquireResult::Failed {
            detail: err.to_string(),
        };
    }

    // 4. Print the token (the caller's job) and exit 0.
    AcquireResult::Acquired {
        token,
        expires_at,
        backup,
    }
}

/// Downloads a binary file's bytes and writes them to `backup_dir`
/// (ADR-0080 §3's binary-file case).
async fn byte_backup(
    files_api: &FilesApi<'_>,
    file_id: &str,
    backup_dir: &Path,
    name: &str,
) -> anyhow::Result<LeaseBackup> {
    let bytes = files_api.download(file_id).await?;
    let sha256 = crate::cli::drive::read::to_hex_string(&Sha256::digest(&bytes));
    let path = backup_file_path(backup_dir, file_id, name);
    write_backup(&path, &bytes)?;
    Ok(LeaseBackup::Bytes {
        path,
        sha256,
        size: bytes.len() as u64,
    })
}

/// Copies a native document into `backup_folder_id` via `files.copy`
/// (ADR-0080 §3's native-document case) — lossless, and restorable by a
/// human in the Drive UI even without this tool.
async fn native_backup(
    files_api: &FilesApi<'_>,
    file_id: &str,
    backup_folder_id: &str,
    name: &str,
) -> anyhow::Result<LeaseBackup> {
    let copy_name = backup_name(file_id, name);
    let copy = files_api
        .copy(file_id, backup_folder_id, &copy_name)
        .await?;
    Ok(LeaseBackup::DriveCopy { file_id: copy.id })
}

/// `<dir>` joined with [`backup_name`]'s result.
fn backup_file_path(dir: &Path, file_id: &str, name: &str) -> PathBuf {
    dir.join(backup_name(file_id, name))
}

/// `<YYYYMMDDTHHMMSSZ>-<fileId>-<name>` (ADR-0080 §3) — UTC, seconds
/// precision, the file id first to survive a name containing `/`. Shared by
/// the local byte-backup path ([`backup_file_path`] joins it under a
/// directory) and the native Drive-copy path (used directly as the copy's
/// own `name`).
fn backup_name(file_id: &str, name: &str) -> String {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let safe_name = name.replace('/', "_");
    format!("{timestamp}-{file_id}-{safe_name}")
}

/// Writes `bytes` to `path`, creating a missing `0700` parent directory
/// first — the same posture `crate::request_log`/the lease ledger use.
///
/// Opened `0600`-from-birth (never `std::fs::write`'s umask-derived mode)
/// — a backup is exactly the private, sensitive content this feature
/// exists to protect. Also `create_new` (`O_EXCL`) rather than a
/// truncating write: [`backup_file_path`]'s name has only whole-second
/// precision, so two acquisitions for the same file within one second
/// collide on the same path. Failing loudly here, instead of silently
/// overwriting, is what stops that rare collision from corrupting the
/// *earlier* lease's ledger row — its recorded `backup_sha256` would
/// otherwise no longer match the bytes actually on disk.
fn write_backup(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            crate::daemon::paths::ensure_dir_0700(parent)?;
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| {
        format!(
            "Failed to create backup file at {} — it may already exist from a near-simultaneous \
             `drive lease acquire` on the same file within the same second; retry",
            path.display()
        )
    })?;
    crate::daemon::paths::ensure_handle_0600(&file)
        .with_context(|| format!("Failed to set 0600 on backup file {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("Failed to write backup to {}", path.display()))?;
    Ok(())
}

/// Inserts `record` into the ledger at `ledger_path` under [`LedgerLock`],
/// loading and saving it in full — the atomic-rewrite contract
/// [`LeaseLedger`] documents.
fn insert_record(record: LeaseRecord, ledger_path: &Path) -> anyhow::Result<()> {
    let _lock = LedgerLock::acquire(ledger_path)?;
    let mut ledger = LeaseLedger::load(ledger_path)?;
    ledger.insert(record);
    ledger.save(ledger_path)
}

/// Builds and writes the `kind: "audit"` record for one acquire attempt.
/// See [`acquire`]'s own doc comment for why this is best-effort rather than
/// write-ahead/fail-closed.
fn record_attempt(opts: &AcquireOptions, result: &AcquireResult) {
    let auth_policy = Some(
        match opts.auth_policy {
            AuthPolicy::DeviceOwner => "device-owner",
            AuthPolicy::BiometricsOnly => "biometrics-only",
        }
        .to_string(),
    );
    let outcome = match result {
        AcquireResult::Acquired {
            token,
            backup,
            expires_at: _,
        } => {
            let (backup_location, backup_sha256, backup_size) = match backup {
                LeaseBackup::Bytes { path, sha256, size } => (
                    Some(path.display().to_string()),
                    Some(sha256.clone()),
                    Some(*size),
                ),
                LeaseBackup::DriveCopy { file_id } => (Some(file_id.clone()), None, None),
            };
            // Re-read the just-written ledger row for the version/
            // modified_time actually recorded, rather than widening
            // `AcquireResult::Acquired` (a public, `--output json` wire
            // shape) with fields that exist only for this best-effort audit
            // record. A read failure here just omits them — the acquisition
            // itself already fully succeeded.
            let (version_after, modified_time_after) = LeaseLedger::load(&opts.ledger_path)
                .ok()
                .and_then(|ledger| ledger.get(token).cloned())
                .map_or((None, None), |record| {
                    (Some(record.version), record.modified_time)
                });
            crate::request_log::AuditOutcome {
                command: vec!["drive".to_string(), "lease-acquire".to_string()],
                integration: "drive",
                file_id: opts.file_id.clone(),
                lease_id: Some(token.clone()),
                verdict: "acquired".to_string(),
                version_after,
                modified_time_after,
                backup_location,
                backup_sha256,
                backup_size,
                auth_policy,
                ..Default::default()
            }
        }
        AcquireResult::RefusedNativeDocument => crate::request_log::AuditOutcome {
            command: vec!["drive".to_string(), "lease-acquire".to_string()],
            integration: "drive",
            file_id: opts.file_id.clone(),
            verdict: "refused-native-document".to_string(),
            auth_policy,
            ..Default::default()
        },
        AcquireResult::Denied { detail } => crate::request_log::AuditOutcome {
            command: vec!["drive".to_string(), "lease-acquire".to_string()],
            integration: "drive",
            file_id: opts.file_id.clone(),
            verdict: "denied".to_string(),
            error: Some(detail.clone()),
            auth_policy,
            ..Default::default()
        },
        AcquireResult::Unavailable { detail } => crate::request_log::AuditOutcome {
            command: vec!["drive".to_string(), "lease-acquire".to_string()],
            integration: "drive",
            file_id: opts.file_id.clone(),
            verdict: "unavailable".to_string(),
            error: Some(detail.clone()),
            auth_policy,
            ..Default::default()
        },
        AcquireResult::Failed { detail } => crate::request_log::AuditOutcome {
            command: vec!["drive".to_string(), "lease-acquire".to_string()],
            integration: "drive",
            file_id: opts.file_id.clone(),
            verdict: "failed".to_string(),
            error: Some(detail.clone()),
            auth_policy,
            ..Default::default()
        },
    };
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("drive lease acquire: failed to write audit record: {err}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::lease::authenticate::Unsupported;
    use crate::test_support::AuditLogGuard as AuditGuard;
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

    struct FakeAuthenticator(AuthOutcome);
    impl Authenticator for FakeAuthenticator {
        fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
            self.0.clone()
        }
    }

    /// Builds options rooted at `dir` (a `tempdir`), so a test never
    /// touches the real backup directory or the real lease ledger.
    fn opts(dir: &Path) -> AcquireOptions {
        AcquireOptions {
            file_id: "f1".to_string(),
            backup_dir: dir.join("backups"),
            native_backup_folder_id: None,
            expiry: ChronoDuration::minutes(30),
            auth_policy: AuthPolicy::DeviceOwner,
            ledger_path: dir.join("lease-ledger.jsonl"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn denied_authentication_takes_no_backup_and_writes_no_ledger_row() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Denied("no".to_string())),
        )
        .await;

        assert!(matches!(result, AcquireResult::Denied { .. }));
        assert!(!root.path().join("backups").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unavailable_authentication_takes_no_backup() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(&client, &opts(root.path()), &Unsupported).await;

        assert!(matches!(result, AcquireResult::Unavailable { .. }));
        assert!(!root.path().join("backups").exists());
    }

    #[tokio::test]
    async fn get_metadata_failure_is_reported_as_failed_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": {"code": 404, "message": "File not found"}
                })),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate when metadata lookup already failed");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_failure_after_authorization_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        assert!(matches!(result, AcquireResult::Failed { .. }));
        assert!(!root.path().join("backups").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_backup_write_collision_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());
        // Force `write_backup`'s `create_new` `open()` to fail deterministically
        // by putting a plain file where the backup directory should be, so the
        // `with_context` closure (otherwise dead in every other test here)
        // actually runs. Pre-creating the *exact* colliding path instead (the
        // same-second collision the doc comment describes) is racy under CI
        // load: the backup filename has only whole-second precision, and
        // enough time can pass between computing it here and `acquire()`
        // computing its own for the two to land in different seconds.
        std::fs::write(&test_opts.backup_dir, b"not a directory").unwrap();

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Failed { detail } = result else {
            panic!("expected Failed, got {result:?}");
        };
        assert!(detail.contains("Failed to create backup file"), "{detail}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ledger_insert_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());
        // A directory in place of the ledger file makes `LeaseLedger::load`
        // fail, which `insert_record` surfaces as an `anyhow::Error`.
        std::fs::create_dir_all(&test_opts.ledger_path).unwrap();

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        assert!(matches!(result, AcquireResult::Failed { .. }));
    }

    #[test]
    fn acquire_result_serializes_to_jsonl() {
        use crate::cli::drive::format::JsonlSerialize;

        let mut buf = Vec::new();
        AcquireResult::Acquired {
            token: "tok-1".to_string(),
            expires_at: Utc::now(),
            backup: LeaseBackup::Bytes {
                path: PathBuf::from("/tmp/backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
        }
        .write_jsonl(&mut buf)
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("\"status\":\"acquired\""), "{text}");
        assert!(text.contains("tok-1"), "{text}");
    }

    #[tokio::test]
    async fn native_document_is_refused_before_authenticating() {
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
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        // An authenticator that panics if called at all — proves the
        // native-document refusal happens before authentication, per the
        // module doc ("mirroring drive edit's identical refusal").
        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate for a Google-native document");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::RefusedNativeDocument));
    }

    #[tokio::test]
    async fn missing_version_is_refused_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf"
                })),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate without a version to lease against");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn authorized_acquisition_backs_up_bytes_and_records_a_ledger_row() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "42", "modifiedTime": "2026-09-11T00:00:00Z"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Acquired { token, backup, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };
        let LeaseBackup::Bytes { path, .. } = backup else {
            panic!("expected a Bytes backup for a binary file, got {backup:?}");
        };
        assert!(!token.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("f1-report.pdf"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recorded_version_reflects_a_post_backup_fetch_not_the_pre_auth_snapshot() {
        // Regression test for the lease-record TOCTOU (issue #1664): the
        // metadata read at the very top of `acquire` (before the
        // authenticator prompt and the backup itself) returns version
        // "0". A foreign edit lands during that window, so the fetch
        // taken right after the backup returns "1" — and "1" is what must
        // end up in the ledger record, not the pre-auth "0" (which would
        // no longer match what was actually backed up).
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "0", "modifiedTime": "2026-09-11T00:00:00Z"
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "1", "modifiedTime": "2026-09-11T00:05:00Z"
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Acquired { token, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        let record = ledger.get(&token).expect("token must be in the ledger");
        assert_eq!(
            record.version, "1",
            "the recorded version must come from the post-backup fetch, not the pre-auth one"
        );
        assert_eq!(
            record.modified_time.as_deref(),
            Some("2026-09-11T00:05:00Z")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn native_document_with_a_backup_folder_configured_copies_instead_of_refusing() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/copy"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "copy-1", "name": "backup", "mimeType": "application/vnd.google-apps.spreadsheet"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut opts = opts(root.path());
        opts.native_backup_folder_id = Some("backup-folder".to_string());

        let result = acquire(&client, &opts, &FakeAuthenticator(AuthOutcome::Authorized)).await;

        let AcquireResult::Acquired { backup, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };
        assert_eq!(
            backup,
            LeaseBackup::DriveCopy {
                file_id: "copy-1".to_string()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_native_copy_after_authorization_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/copy"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut opts = opts(root.path());
        opts.native_backup_folder_id = Some("backup-folder".to_string());

        let result = acquire(&client, &opts, &FakeAuthenticator(AuthOutcome::Authorized)).await;

        assert!(
            matches!(result, AcquireResult::Failed { .. }),
            "expected Failed, got {result:?}"
        );
        assert!(!opts.ledger_path.exists(), "no ledger row must be written");
    }

    /// Mounts the pre-auth `files.get` (version "1", consumed once) plus
    /// the byte download, leaving the post-backup `files.get` to the
    /// caller — for tests exercising a failure in that second fetch.
    async fn mount_pre_auth_metadata_and_download(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "1"
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_post_backup_metadata_fetch_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        mount_pre_auth_metadata_and_download(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        assert!(
            matches!(result, AcquireResult::Failed { .. }),
            "expected Failed, got {result:?}"
        );
        assert!(
            !test_opts.ledger_path.exists(),
            "no ledger row must be written"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_post_backup_fetch_without_a_version_is_reported_as_failed() {
        // The pre-auth fetch carried a `version`, so the early refusal
        // does not fire; the post-backup fetch — the one the ledger row is
        // actually recorded from — omits it, and a lease with no staleness
        // check must not be minted.
        let server = wiremock::MockServer::start().await;
        mount_pre_auth_metadata_and_download(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf"
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Failed { detail } = result else {
            panic!("expected Failed, got {result:?}");
        };
        assert!(detail.contains("after the backup"), "{detail}");
        assert!(
            !test_opts.ledger_path.exists(),
            "no ledger row must be written"
        );
    }

    // ── the audit sink (ADR-0080 §11) ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn an_acquired_lease_writes_an_audit_record_carrying_the_backup_and_version() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "42", "modifiedTime": "2026-09-11T00:00:00Z"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Acquired { token, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };

        let contents = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
        let rec: crate::request_log::LogRecord = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(rec.kind, crate::request_log::RecordKind::Audit);
        assert_eq!(rec.context.get("lease_id"), Some(&token));
        assert_eq!(
            rec.context.get("verdict").map(String::as_str),
            Some("acquired")
        );
        assert_eq!(
            rec.context.get("version_after").map(String::as_str),
            Some("42")
        );
        assert_eq!(
            rec.context.get("auth_policy").map(String::as_str),
            Some("device-owner")
        );
        assert!(rec.context.contains_key("backup_sha256"));
    }

    #[tokio::test]
    async fn a_refused_acquisition_still_writes_an_audit_record() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate with no backup folder configured");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;

        assert!(matches!(result, AcquireResult::RefusedNativeDocument));

        let contents = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
        let rec: crate::request_log::LogRecord = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(
            rec.context.get("verdict").map(String::as_str),
            Some("refused-native-document")
        );
        assert_eq!(rec.context.get("lease_id"), None);
    }

    #[tokio::test]
    async fn native_document_without_a_backup_folder_is_refused_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate with no backup folder configured");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::RefusedNativeDocument));
    }
}
