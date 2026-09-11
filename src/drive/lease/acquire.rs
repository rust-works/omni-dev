//! `drive lease acquire` — the engine behind
//! [ADR-0080](../../../docs/adrs/adr-0080.md) §2: authenticate, then back
//! up, then record, then mint. Binary files only in this phase (§3's
//! native-file `files.copy` case lands with Phase 3); a Google-native
//! target is refused client-side, before any of the four steps, mirroring
//! `drive edit`'s identical refusal.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cli::drive::format::JsonlSerialize;
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::authenticate::{AuthOutcome, AuthPolicy, Authenticator};
use crate::drive::lease::ledger::{LeaseLedger, LeaseRecord, LedgerLock};

/// Per-call options for `drive lease acquire`.
#[derive(Debug, Clone)]
pub struct AcquireOptions {
    /// The file id to lease.
    pub file_id: String,
    /// Local directory byte backups are written under.
    pub backup_dir: PathBuf,
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
        backup_path: PathBuf,
    },
    /// The target is a Google-native document — no bytes to back up this
    /// way. Native-file leases land with Phase 3.
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
/// the first failure with nothing further attempted.
pub async fn acquire(
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

    if target.is_google_native() {
        return AcquireResult::RefusedNativeDocument;
    }

    let Some(version) = target.version.clone() else {
        return AcquireResult::Failed {
            detail: "Drive did not return a `version` for this file; refusing to lease it \
                     without a staleness check"
                .to_string(),
        };
    };

    // 1. Authenticate — consent gates the action, not merely possession of
    // the resulting token (ADR-0080 §2). Nothing below runs on refusal.
    let reason = format!("back up and lease-write '{}'", target.name);
    match authenticator.authenticate(&reason, opts.auth_policy) {
        AuthOutcome::Authorized => {}
        AuthOutcome::Denied(detail) => return AcquireResult::Denied { detail },
        AuthOutcome::Unavailable(detail) => return AcquireResult::Unavailable { detail },
    }

    // 2. Backup.
    let bytes = match files_api.download(&opts.file_id).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return AcquireResult::Failed {
                detail: err.to_string(),
            }
        }
    };
    let sha256 = sha256_hex(&bytes);
    let backup_path = backup_file_path(&opts.backup_dir, &opts.file_id, &target.name);
    if let Err(err) = write_backup(&backup_path, &bytes) {
        return AcquireResult::Failed {
            detail: err.to_string(),
        };
    }

    // 3. Ledger record.
    let token = crate::request_log::new_id();
    let now = Utc::now();
    let expires_at = now + opts.expiry;
    let record = LeaseRecord {
        token: token.clone(),
        file_id: opts.file_id.clone(),
        version,
        modified_time: target.modified_time.clone(),
        backup_path: backup_path.clone(),
        backup_sha256: sha256,
        backup_size: bytes.len() as u64,
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
        backup_path,
    }
}

/// SHA-256 of `bytes`, as lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// `<dir>/<YYYYMMDDTHHMMSSZ>-<fileId>-<name>` (ADR-0080 §3) — UTC, seconds
/// precision, the file id first to survive a name containing `/`.
fn backup_file_path(dir: &Path, file_id: &str, name: &str) -> PathBuf {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let safe_name = name.replace('/', "_");
    dir.join(format!("{timestamp}-{file_id}-{safe_name}"))
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::lease::authenticate::Unsupported;
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
            expiry: ChronoDuration::minutes(30),
            auth_policy: AuthPolicy::DeviceOwner,
            ledger_path: dir.join("lease-ledger.jsonl"),
        }
    }

    #[tokio::test]
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

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Denied("no".to_string())),
        )
        .await;

        assert!(matches!(result, AcquireResult::Denied { .. }));
        assert!(!root.path().join("backups").exists());
    }

    #[tokio::test]
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

        let result = acquire(&client, &opts(root.path()), &Unsupported).await;

        assert!(matches!(result, AcquireResult::Unavailable { .. }));
        assert!(!root.path().join("backups").exists());
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

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate without a version to lease against");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::Failed { .. }));
    }

    #[tokio::test]
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

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Acquired {
            token, backup_path, ..
        } = result
        else {
            panic!("expected Acquired, got {result:?}");
        };
        assert!(!token.is_empty());
        assert_eq!(std::fs::read(&backup_path).unwrap(), b"hello");
        assert!(backup_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("f1-report.pdf"));
    }
}
