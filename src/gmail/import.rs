//! Imports a Google Cloud OAuth2 client id/secret from `client_secret.json`.
//!
//! Reads the file the Cloud Console hands out directly, so the secret never
//! has to transit a shell, an env var, or an agent's context on its way into
//! `~/.omni-dev/settings.json`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::gmail::account::ResolvedAccount;
use crate::gmail::auth::{self, GMAIL_CLIENT_ID, GMAIL_CLIENT_SECRET};
use crate::utils::env::{EnvSource, SystemEnv};
use crate::utils::secret::Secret;
use crate::utils::settings::{active_profile_from, Settings};

/// Environment variable naming an explicit `client_secret.json` path,
/// consulted by [`discover_client_secret_file`] when no `PATH` argument is
/// given.
pub const GMAIL_CLIENT_SECRET_FILE: &str = "GMAIL_CLIENT_SECRET_FILE";

/// A parsed, not-yet-persisted OAuth2 client id/secret.
#[derive(Debug, Clone)]
pub struct ImportedClientCredentials {
    /// OAuth2 client id (not secret).
    pub client_id: String,
    /// OAuth2 client secret (redacted in `Debug` output).
    pub client_secret: Secret,
}

/// The result of a successful import — enough for the CLI layer to report
/// what happened without ever handling the secret itself.
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    /// The `client_secret.json` path that was used.
    pub path: PathBuf,
    /// The imported client id (safe to print).
    pub client_id: String,
}

/// Discovers, parses, and saves a `client_secret.json`'s client id/secret to
/// `~/.omni-dev/settings.json`.
pub fn import_client_credentials(explicit: Option<&Path>) -> Result<ImportOutcome> {
    import_client_credentials_to(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
        &SystemEnv,
        dirs::home_dir().as_deref(),
        explicit,
    )
}

/// [`import_client_credentials`], writing to an explicit settings-file path
/// and reading from an injected [`EnvSource`]/home directory — the test seam
/// so discovery and persistence are exercised without touching `HOME` or the
/// process environment.
pub(crate) fn import_client_credentials_to(
    settings_path: &Path,
    profile: Option<&str>,
    env: &impl EnvSource,
    home: Option<&Path>,
    explicit: Option<&Path>,
) -> Result<ImportOutcome> {
    let path = discover_client_secret_file(env, home, explicit)?;
    let credentials = parse_client_secret_file(&path)?;
    save_client_credentials_to(settings_path, profile, &credentials)?;
    Ok(ImportOutcome {
        path,
        client_id: credentials.client_id,
    })
}

/// [`import_client_credentials`], but honoring the named-account resolution
/// added by issue #1500.
///
/// `explicit_account` is the already-resolved `--account`/
/// [`crate::gmail::account::GMAIL_ACCOUNT_ENV`] override, if any — resolved
/// via `auth::resolve_for_write`, so an explicit name need not already be
/// configured (this is how a new account is created); `explicit_path` is
/// the `client_secret.json` path argument, resolved exactly as in
/// [`import_client_credentials`].
pub fn import_client_credentials_for(
    explicit_account: Option<&str>,
    explicit_path: Option<&Path>,
) -> Result<ImportOutcome> {
    let path = discover_client_secret_file(&SystemEnv, dirs::home_dir().as_deref(), explicit_path)?;
    let credentials = parse_client_secret_file(&path)?;

    let settings = Settings::load().unwrap_or_default();
    match auth::resolve_for_write(&settings.gmail, explicit_account)? {
        ResolvedAccount::Legacy => save_client_credentials_to(
            &Settings::get_settings_path()?,
            active_profile_from(&SystemEnv).as_deref(),
            &credentials,
        )?,
        ResolvedAccount::Named(name) => Settings::upsert_gmail_account(
            &Settings::get_settings_path()?,
            &name,
            &[
                ("client_id", credentials.client_id.as_str()),
                ("client_secret", credentials.client_secret.expose_secret()),
            ],
        )?,
    }

    Ok(ImportOutcome {
        path,
        client_id: credentials.client_id,
    })
}

/// Finds a `client_secret.json` by trying, in order: an explicit path, the
/// `GMAIL_CLIENT_SECRET_FILE` environment variable, `~/.config/gws/client_secret.json`,
/// and the most-recently-modified
/// `~/Downloads/client_secret_*.apps.googleusercontent.com.json` (the Cloud
/// Console's default download filename).
///
/// An explicit path or env var that names a file that doesn't exist is a
/// hard error, not a silent fall-through — the user named it directly.
pub(crate) fn discover_client_secret_file(
    env: &impl EnvSource,
    home: Option<&Path>,
    explicit: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return if path.exists() {
            Ok(path.to_path_buf())
        } else {
            Err(anyhow!("{} does not exist", path.display()))
        };
    }

    if let Some(raw) = env.var(GMAIL_CLIENT_SECRET_FILE) {
        let path = PathBuf::from(&raw);
        return if path.exists() {
            Ok(path)
        } else {
            Err(anyhow!(
                "GMAIL_CLIENT_SECRET_FILE is set to {} but that file does not exist",
                path.display()
            ))
        };
    }

    if let Some(home) = home {
        let gws_path = home.join(".config").join("gws").join("client_secret.json");
        if gws_path.exists() {
            return Ok(gws_path);
        }

        if let Some(found) = find_downloaded_client_secret(&home.join("Downloads")) {
            return Ok(found);
        }
    }

    Err(anyhow!(
        "No client_secret.json found. Tried $GMAIL_CLIENT_SECRET_FILE, \
         ~/.config/gws/client_secret.json, and \
         ~/Downloads/client_secret_*.apps.googleusercontent.com.json.\n\
         Pass an explicit path instead: `omni-dev gmail auth import <PATH>` \
         (see docs/gmail.md)."
    ))
}

/// Scans `dir` for `client_secret_*.apps.googleusercontent.com.json` —
/// Google Cloud Console's default download filename — and returns the
/// most-recently-modified match, if any. No `glob` crate dependency exists
/// in this repo, so this is a manual prefix/suffix filter over `read_dir`,
/// matching the pattern already used elsewhere for filename-filtered
/// directory scans (e.g. `crate::claude::context::discovery`).
fn find_downloaded_client_secret(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;

    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            name.starts_with("client_secret_") && name.ends_with(".apps.googleusercontent.com.json")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

/// The two client-type shapes Google's console can hand out. Deliberately
/// keeps both optional at the top level so genuinely malformed JSON fails
/// inside `serde_json::from_str` itself (a clear parse error), while
/// well-formed-but-wrong-shaped JSON (neither key present) falls through to
/// an explicit, actionable match in [`parse_client_secret_file`].
#[derive(Debug, Deserialize)]
struct ClientSecretFile {
    #[serde(default)]
    installed: Option<ClientSecretEntry>,
    #[serde(default)]
    web: Option<ClientSecretEntry>,
}

#[derive(Debug, Deserialize)]
struct ClientSecretEntry {
    client_id: String,
    client_secret: String,
}

/// Parses a `client_secret.json` file, rejecting a "Web application" client
/// (`web`) since Gmail login's loopback redirect requires a "Desktop app"
/// client (`installed`) — see [ADR-0063](../../docs/adrs/adr-0063.md).
pub(crate) fn parse_client_secret_file(path: &Path) -> Result<ImportedClientCredentials> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let parsed: ClientSecretFile = serde_json::from_str(&content)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    match (parsed.installed, parsed.web) {
        (Some(entry), _) => Ok(ImportedClientCredentials {
            client_id: entry.client_id,
            client_secret: Secret::new(entry.client_secret),
        }),
        (None, Some(_)) => Err(anyhow!(
            "{} is a \"Web application\" OAuth client, but Gmail login needs a \"Desktop app\" \
             client — a Web application client can't do the loopback redirect Gmail login uses \
             (its redirect URIs must be pre-registered, port included). Create a Desktop app \
             client in Google Cloud Console instead (see docs/gmail.md).",
            path.display()
        )),
        (None, None) => Err(anyhow!(
            "{} does not look like a Google OAuth client_secret.json (missing top-level \
             \"installed\" or \"web\" key).",
            path.display()
        )),
    }
}

/// Saves the imported client id/secret to `~/.omni-dev/settings.json` —
/// only the two pre-login keys; `GMAIL_REFRESH_TOKEN`/`GMAIL_SCOPE` are
/// written later by a successful `auth login`.
fn save_client_credentials_to(
    settings_path: &Path,
    profile: Option<&str>,
    credentials: &ImportedClientCredentials,
) -> Result<()> {
    Settings::upsert_env_vars_in(
        settings_path,
        profile,
        &[
            (GMAIL_CLIENT_ID, credentials.client_id.as_str()),
            (
                GMAIL_CLIENT_SECRET,
                credentials.client_secret.expose_secret(),
            ),
        ],
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    fn temp_dir() -> tempfile::TempDir {
        std::fs::create_dir_all("tmp").ok();
        tempfile::TempDir::new_in("tmp").unwrap()
    }

    fn write_installed_json(path: &Path, client_id: &str, client_secret: &str) {
        std::fs::write(
            path,
            serde_json::json!({
                "installed": {
                    "client_id": client_id,
                    "client_secret": client_secret,
                    "project_id": "test-project",
                    "auth_uri": "https://accounts.google.com/o/oauth2/auth",
                    "token_uri": "https://oauth2.googleapis.com/token",
                    "redirect_uris": ["http://localhost"],
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn write_web_json(path: &Path) {
        std::fs::write(
            path,
            serde_json::json!({
                "web": {
                    "client_id": "web-id",
                    "client_secret": "web-secret",
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    // ── parse_client_secret_file ─────────────────────────────────────

    #[test]
    fn parse_accepts_installed_client() {
        let dir = temp_dir();
        let path = dir.path().join("client_secret.json");
        write_installed_json(&path, "the-id", "the-secret");

        let creds = parse_client_secret_file(&path).unwrap();
        assert_eq!(creds.client_id, "the-id");
        assert_eq!(creds.client_secret.expose_secret(), "the-secret");
    }

    #[test]
    fn parse_rejects_web_client_naming_desktop_app_requirement() {
        let dir = temp_dir();
        let path = dir.path().join("client_secret.json");
        write_web_json(&path);

        let err = parse_client_secret_file(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Web application"));
        assert!(msg.contains("Desktop app"));
    }

    #[test]
    fn parse_rejects_malformed_json() {
        let dir = temp_dir();
        let path = dir.path().join("client_secret.json");
        std::fs::write(&path, "{ not json").unwrap();

        assert!(parse_client_secret_file(&path).is_err());
    }

    #[test]
    fn parse_rejects_valid_json_with_neither_key() {
        let dir = temp_dir();
        let path = dir.path().join("client_secret.json");
        std::fs::write(&path, serde_json::json!({"foo": "bar"}).to_string()).unwrap();

        let err = parse_client_secret_file(&path).unwrap_err();
        assert!(err.to_string().contains("installed"));
    }

    #[test]
    fn parse_rejects_absent_file() {
        let dir = temp_dir();
        let path = dir.path().join("does-not-exist.json");
        assert!(parse_client_secret_file(&path).is_err());
    }

    #[test]
    fn imported_client_credentials_debug_redacts_client_secret() {
        let creds = ImportedClientCredentials {
            client_id: "the-id".to_string(),
            client_secret: Secret::new("super-secret-value"),
        };
        let debug = format!("{creds:?}");
        assert!(debug.contains("the-id"));
        assert!(!debug.contains("super-secret-value"));
    }

    // ── discover_client_secret_file ──────────────────────────────────

    #[test]
    fn discover_prefers_explicit_path_over_everything() {
        let dir = temp_dir();
        let explicit_path = dir.path().join("explicit.json");
        write_installed_json(&explicit_path, "id", "secret");
        let env = MapEnv::new().with(GMAIL_CLIENT_SECRET_FILE, "/should/not/be/used.json");

        let found = discover_client_secret_file(&env, None, Some(&explicit_path)).unwrap();
        assert_eq!(found, explicit_path);
    }

    #[test]
    fn discover_errors_when_explicit_path_does_not_exist() {
        let env = MapEnv::new();
        let missing = PathBuf::from("/definitely/does/not/exist.json");
        let err = discover_client_secret_file(&env, None, Some(&missing)).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn discover_uses_env_var_when_no_explicit_path() {
        let dir = temp_dir();
        let env_path = dir.path().join("from-env.json");
        write_installed_json(&env_path, "id", "secret");
        let env = MapEnv::new().with(GMAIL_CLIENT_SECRET_FILE, env_path.to_str().unwrap());

        let found = discover_client_secret_file(&env, None, None).unwrap();
        assert_eq!(found, env_path);
    }

    #[test]
    fn discover_errors_when_env_var_path_does_not_exist() {
        let env = MapEnv::new().with(GMAIL_CLIENT_SECRET_FILE, "/definitely/does/not/exist.json");
        let err = discover_client_secret_file(&env, None, None).unwrap_err();
        assert!(err.to_string().contains("GMAIL_CLIENT_SECRET_FILE"));
    }

    #[test]
    fn discover_falls_back_to_gws_path() {
        let dir = temp_dir();
        let gws_dir = dir.path().join(".config").join("gws");
        std::fs::create_dir_all(&gws_dir).unwrap();
        let gws_path = gws_dir.join("client_secret.json");
        write_installed_json(&gws_path, "id", "secret");
        let env = MapEnv::new();

        let found = discover_client_secret_file(&env, Some(dir.path()), None).unwrap();
        assert_eq!(found, gws_path);
    }

    #[test]
    fn discover_falls_back_to_downloads_glob() {
        let dir = temp_dir();
        let downloads = dir.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let dl_path = downloads.join("client_secret_123.apps.googleusercontent.com.json");
        write_installed_json(&dl_path, "id", "secret");
        let env = MapEnv::new();

        let found = discover_client_secret_file(&env, Some(dir.path()), None).unwrap();
        assert_eq!(found, dl_path);
    }

    #[test]
    fn discover_downloads_glob_picks_most_recently_modified_on_multiple_matches() {
        let dir = temp_dir();
        let downloads = dir.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();

        let older = downloads.join("client_secret_1.apps.googleusercontent.com.json");
        write_installed_json(&older, "old-id", "old-secret");
        let newer = downloads.join("client_secret_2.apps.googleusercontent.com.json");
        write_installed_json(&newer, "new-id", "new-secret");

        // Ensure a real mtime gap regardless of filesystem timestamp
        // resolution.
        let now = std::time::SystemTime::now();
        std::fs::File::open(&older)
            .unwrap()
            .set_modified(now - std::time::Duration::from_secs(60))
            .unwrap();
        std::fs::File::open(&newer)
            .unwrap()
            .set_modified(now)
            .unwrap();

        let env = MapEnv::new();
        let found = discover_client_secret_file(&env, Some(dir.path()), None).unwrap();
        assert_eq!(found, newer);
    }

    #[test]
    fn discover_errors_naming_all_tried_locations_when_nothing_found() {
        let dir = temp_dir();
        let env = MapEnv::new();
        let err = discover_client_secret_file(&env, Some(dir.path()), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("GMAIL_CLIENT_SECRET_FILE"));
        assert!(msg.contains("gws"));
        assert!(msg.contains("Downloads"));
    }

    // ── import_client_credentials_to ─────────────────────────────────

    #[test]
    fn import_writes_only_client_id_and_secret() {
        let dir = temp_dir();
        let source_path = dir.path().join("client_secret.json");
        write_installed_json(&source_path, "imported-id", "imported-secret");
        let settings_path = dir.path().join("settings.json");
        let env = MapEnv::new();

        let outcome =
            import_client_credentials_to(&settings_path, None, &env, None, Some(&source_path))
                .unwrap();

        assert_eq!(outcome.client_id, "imported-id");
        assert_eq!(outcome.path, source_path);

        let saved = std::fs::read_to_string(&settings_path).unwrap();
        assert!(saved.contains("imported-id"));
        assert!(saved.contains("imported-secret"));
        assert!(!saved.contains("GMAIL_REFRESH_TOKEN"));
        assert!(!saved.contains("GMAIL_SCOPE"));
    }

    #[test]
    fn import_rejects_web_client_and_leaves_settings_untouched() {
        let dir = temp_dir();
        let source_path = dir.path().join("client_secret.json");
        write_web_json(&source_path);
        let settings_path = dir.path().join("settings.json");
        let env = MapEnv::new();

        let err =
            import_client_credentials_to(&settings_path, None, &env, None, Some(&source_path))
                .unwrap_err();

        assert!(err.to_string().contains("Desktop app"));
        assert!(!settings_path.exists());
    }

    #[test]
    fn import_targets_the_active_profile_env_map() {
        let dir = temp_dir();
        let source_path = dir.path().join("client_secret.json");
        write_installed_json(&source_path, "profile-id", "profile-secret");
        let settings_path = dir.path().join("settings.json");
        let env = MapEnv::new();

        import_client_credentials_to(&settings_path, Some("work"), &env, None, Some(&source_path))
            .unwrap();

        let saved = std::fs::read_to_string(&settings_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            value["profiles"]["work"]["env"]["GMAIL_CLIENT_ID"],
            "profile-id"
        );
    }

    // ── import_client_credentials_for (issue #1500) ─────────────────────
    //
    // These exercise the production wrapper, which resolves the settings
    // path from `HOME` (like `import_client_credentials` itself), so they
    // redirect it via `EnvGuard`. Passing an explicit `client_secret.json`
    // path bypasses discovery entirely, so no env/home mocking is needed
    // for that half.

    #[test]
    fn import_for_named_account_writes_gmail_accounts_not_env() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_gmail_account(&settings_path, "work", &[("client_id", "placeholder")])
            .unwrap();

        let secret_dir = temp_dir();
        let secret_path = secret_dir.path().join("client_secret.json");
        write_installed_json(&secret_path, "the-id", "the-secret");

        let outcome = import_client_credentials_for(Some("work"), Some(&secret_path)).unwrap();
        assert_eq!(outcome.client_id, "the-id");

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["gmail"]["accounts"]["work"]["client_id"], "the-id");
        assert_eq!(
            val["gmail"]["accounts"]["work"]["client_secret"],
            "the-secret"
        );
        assert!(val.get("env").is_none());
    }

    #[test]
    fn import_for_creates_brand_new_named_account_without_prior_validation() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        // No account named "fresh" exists yet — import must still succeed,
        // since import is how a new named account is created.

        let secret_dir = temp_dir();
        let secret_path = secret_dir.path().join("client_secret.json");
        write_installed_json(&secret_path, "the-id", "the-secret");

        import_client_credentials_for(Some("fresh"), Some(&secret_path)).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["gmail"]["accounts"]["fresh"]["client_id"], "the-id");
    }

    #[test]
    fn import_for_legacy_when_no_account_given_and_none_configured() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");

        let secret_dir = temp_dir();
        let secret_path = secret_dir.path().join("client_secret.json");
        write_installed_json(&secret_path, "the-id", "the-secret");

        import_client_credentials_for(None, Some(&secret_path)).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["env"]["GMAIL_CLIENT_ID"], "the-id");
        assert!(val.get("gmail").is_none());
    }
}
