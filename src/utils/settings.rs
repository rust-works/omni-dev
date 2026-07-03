//! Settings and configuration utilities.
//!
//! This module provides functionality to read settings from $HOME/.omni-dev/settings.json
//! and use them as a fallback for environment variables.
//!
//! It also owns the write side: [`Settings::upsert_env_vars`] and
//! [`Settings::remove_env_vars`] are the only production paths that mutate the
//! settings file. Because the `env` map holds credentials (Atlassian, Datadog),
//! every write is hardened: parent directory `0700`, file `0600`, re-tightened
//! on each write (issue #1128).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::utils::env::{EnvSource, SystemEnv};

/// Environment variable that selects the active profile, mirroring `AWS_PROFILE`.
///
/// Read from the **raw** process environment only (never through the profile
/// fallback, which would be circular); the `--profile` flag propagates its value
/// here in `Cli::propagate_global_flags`.
pub const PROFILE_ENV_VAR: &str = "OMNI_DEV_PROFILE";

/// A named credential/config bundle inside `settings.json` — its own `env` map,
/// selected per invocation via `--profile` / `OMNI_DEV_PROFILE`.
#[derive(Debug, Default, Deserialize)]
pub struct Profile {
    /// Environment variable overrides applied when this profile is active.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Settings loaded from $HOME/.omni-dev/settings.json.
#[derive(Debug, Default, Deserialize)]
pub struct Settings {
    /// Environment variable overrides — the default bundle, consulted only when
    /// **no** profile is active.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Named profiles. Selecting one replaces the base `env` in the fallback
    /// chain (isolated / AWS-faithful); see [`Settings::resolve_with`].
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

/// Returns the active profile name from `raw` (the process environment), or
/// `None` when `OMNI_DEV_PROFILE` is unset or empty.
///
/// Reads the **raw** env only, so it is pure over the injected source and never
/// resolves through the profile fallback.
pub fn active_profile_from<E: EnvSource>(raw: &E) -> Option<String> {
    raw.var(PROFILE_ENV_VAR).filter(|s| !s.is_empty())
}

/// An [`EnvSource`](crate::utils::env::EnvSource) with the settings/profile
/// fallback — the value form of [`get_env_var`].
///
/// Reads the real process environment first, then the active profile's `env`
/// (or the base `env` when no profile is active) in
/// `$HOME/.omni-dev/settings.json`.
///
/// Pass `&SettingsEnv::load()` from a thin production wrapper; tests inject a
/// pure `MapEnv` into the same `*_with(&impl EnvSource, …)` seam instead of
/// mutating the process environment.
#[derive(Debug, Default)]
pub struct SettingsEnv {
    settings: Settings,
    active_profile: Option<String>,
}

impl SettingsEnv {
    /// Loads settings from the default location, falling back to an empty
    /// settings map if they are absent or unreadable (env-only behaviour). The
    /// active profile is read from `OMNI_DEV_PROFILE`.
    pub fn load() -> Self {
        Self::load_with_profile(active_profile_from(&SystemEnv).as_deref())
    }

    /// Like [`load`](Self::load) but with the active profile supplied
    /// explicitly — for tests and embedders that select a profile without
    /// setting `OMNI_DEV_PROFILE` in the process environment.
    pub fn load_with_profile(profile: Option<&str>) -> Self {
        Self {
            settings: Settings::load().unwrap_or_default(),
            active_profile: profile.map(str::to_string),
        }
    }
}

impl EnvSource for SettingsEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.settings
            .resolve_with(&SystemEnv, self.active_profile.as_deref(), key)
    }
}

impl Settings {
    /// Loads settings from the default location.
    pub fn load() -> Result<Self> {
        let settings_path = Self::get_settings_path()?;
        Self::load_from_path(&settings_path)
    }

    /// Loads settings from a specific path.
    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        // If file doesn't exist, return default settings
        if !path.exists() {
            return Ok(Self::default());
        }

        // Read and parse the settings file
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read settings file: {}", path.display()))?;

        serde_json::from_str::<Self>(&content)
            .with_context(|| format!("Failed to parse settings file: {}", path.display()))
    }

    /// Returns the default settings path.
    pub fn get_settings_path() -> Result<PathBuf> {
        let home_dir = dirs::home_dir().context("Failed to determine home directory")?;

        Ok(home_dir.join(".omni-dev").join("settings.json"))
    }

    /// Returns an environment variable with fallback to settings, honouring the
    /// active profile from `OMNI_DEV_PROFILE`.
    pub fn get_env_var(&self, key: &str) -> Option<String> {
        self.resolve_with(&SystemEnv, active_profile_from(&SystemEnv).as_deref(), key)
    }

    /// Isolated / AWS-faithful resolution: `raw` (the process environment) wins;
    /// then the active profile's `env` if `active` is set, else the base `env`.
    /// The base map is **not** consulted when a profile is active, so a missing
    /// key fails loud rather than silently reusing a default credential against
    /// the wrong tenant.
    ///
    /// This is the pure seam: production wrappers pass `&SystemEnv`; tests pass
    /// a `MapEnv` and an explicit `active`, mutating no process-global state.
    pub fn resolve_with<E: EnvSource>(
        &self,
        raw: &E,
        active: Option<&str>,
        key: &str,
    ) -> Option<String> {
        if let Some(value) = raw.var(key) {
            return Some(value);
        }
        match active {
            Some(name) => self
                .profiles
                .get(name)
                .and_then(|p| p.env.get(key).cloned()),
            None => self.env.get(key).cloned(),
        }
    }

    /// Merges the given key/value pairs into the `env` object of the settings
    /// file at `path`, creating the file and its parent directory as needed.
    ///
    /// The file is read and written as a generic JSON value, so every other
    /// field (profiles, unknown keys) is preserved verbatim. Because the `env`
    /// map holds credentials, the write is hardened: parent directory `0700`,
    /// file `0600` (see [`write_settings`]).
    pub fn upsert_env_vars(path: &Path, vars: &[(&str, &str)]) -> Result<()> {
        let mut settings_value = read_or_default_settings(path)?;

        // Ensure the "env" key exists as an object
        if !settings_value
            .get("env")
            .is_some_and(serde_json::Value::is_object)
        {
            settings_value["env"] = serde_json::json!({});
        }

        // Merge keys — safe because we just ensured "env" is an object above
        let Some(env) = settings_value["env"].as_object_mut() else {
            anyhow::bail!("Internal error: env key is not an object after initialization");
        };
        for (key, value) in vars {
            env.insert(
                (*key).to_string(),
                serde_json::Value::String((*value).to_string()),
            );
        }

        write_settings(path, &settings_value)
    }

    /// Removes the given keys from the `env` object of the settings file at
    /// `path`, leaving all other settings intact.
    ///
    /// Returns `true` if any key was present and removed (the file is
    /// rewritten, hardened as in [`Settings::upsert_env_vars`]), `false` when
    /// the file did not exist or contained none of the keys (the file is left
    /// untouched).
    pub fn remove_env_vars(path: &Path, keys: &[&str]) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        let mut settings_value = read_or_default_settings(path)?;

        let mut removed = false;
        if let Some(env) = settings_value
            .get_mut("env")
            .and_then(serde_json::Value::as_object_mut)
        {
            for key in keys {
                if env.remove(*key).is_some() {
                    removed = true;
                }
            }
        }

        if removed {
            write_settings(path, &settings_value)?;
        }
        Ok(removed)
    }

    /// Validates that `name` is a known profile, returning a hard error that
    /// lists the known profiles (sorted) otherwise. Called once at the CLI
    /// boundary so a typo never silently falls back to base credentials.
    pub fn validate_profile(&self, name: &str) -> Result<()> {
        if self.profiles.contains_key(name) {
            return Ok(());
        }
        let known = if self.profiles.is_empty() {
            "(none)".to_string()
        } else {
            let mut names: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
            names.sort_unstable();
            names.join(", ")
        };
        Err(anyhow::anyhow!(
            "unknown profile '{name}'; known profiles: {known}"
        ))
    }
}

/// Reads and parses the settings file at `path` as a generic JSON value
/// (preserving unknown fields), or returns `{}` when the file does not exist.
fn read_or_default_settings(path: &Path) -> Result<serde_json::Value> {
    if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))
    } else {
        Ok(serde_json::json!({}))
    }
}

/// The single hardened write site for the settings file: creates the parent
/// directory `0700`, writes the pretty-printed JSON through a `0600` handle
/// (no window where a fresh file is world-readable), and re-tightens a
/// pre-existing looser-permission file on every write (issue #1128).
fn write_settings(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::daemon::paths::ensure_dir_0700(parent)?;
        }
    }
    let formatted =
        serde_json::to_string_pretty(value).context("Failed to serialize settings JSON")?;
    write_file_0600(path, &formatted)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    crate::daemon::paths::set_file_0600(path)?;
    Ok(())
}

/// Creates/truncates `path` with owner-only (`0600`) permissions on Unix.
#[cfg(unix)]
fn write_file_0600(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

/// Non-Unix fallback: a plain write ([`set_file_0600`](crate::daemon::paths::set_file_0600)
/// is a no-op there too).
#[cfg(not(unix))]
fn write_file_0600(path: &Path, contents: &str) -> std::io::Result<()> {
    fs::write(path, contents)
}

/// Returns an environment variable with fallback to settings, honouring the
/// active profile from `OMNI_DEV_PROFILE`.
pub fn get_env_var(key: &str) -> Result<String> {
    get_env_var_with(&SystemEnv, Settings::load, key)
}

/// Pure core of [`get_env_var`]: `env` is the raw source and `load` produces the
/// settings lazily — it is invoked only on a raw-env miss, preserving the
/// no-disk fast path. Tests inject a `MapEnv` and a closure returning `Ok`/`Err`
/// to cover both the resolved and load-failure branches without touching disk.
fn get_env_var_with<E, F>(env: &E, load: F, key: &str) -> Result<String>
where
    E: EnvSource,
    F: FnOnce() -> Result<Settings>,
{
    // A raw process-env hit short-circuits without loading settings from disk.
    if let Some(value) = env.var(key) {
        return Ok(value);
    }
    match load() {
        Ok(settings) => settings
            .resolve_with(env, active_profile_from(env).as_deref(), key)
            .ok_or_else(|| anyhow::anyhow!("Environment variable not found: {key}")),
        Err(err) => {
            // If we couldn't load settings, just return the original env var error
            Err(anyhow::anyhow!("Environment variable not found: {key}").context(err))
        }
    }
}

/// Tries multiple environment variables with fallback to settings.
pub fn get_env_vars(keys: &[&str]) -> Result<String> {
    for key in keys {
        if let Ok(value) = get_env_var(key) {
            return Ok(value);
        }
    }

    Err(anyhow::anyhow!(
        "None of the environment variables found: {keys:?}"
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;
    use std::env;
    use std::fs;
    use tempfile::TempDir;

    /// Builds a `Settings` with a base `env` and one profile, for the pure
    /// resolver tests (no disk, no process env).
    fn settings_with_profile() -> Settings {
        let mut base = HashMap::new();
        base.insert("ATLASSIAN_EMAIL".to_string(), "base@x.com".to_string());
        base.insert("SHARED".to_string(), "base-shared".to_string());

        let mut work_env = HashMap::new();
        work_env.insert("ATLASSIAN_EMAIL".to_string(), "me@work.com".to_string());

        let mut profiles = HashMap::new();
        profiles.insert("work".to_string(), Profile { env: work_env });

        Settings {
            env: base,
            profiles,
        }
    }

    #[test]
    fn settings_load_from_path() {
        // Create a temporary directory (use current dir to avoid TMPDIR issues in tarpaulin)
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            TempDir::new_in("tmp").unwrap()
        };
        let settings_path = temp_dir.path().join("settings.json");

        // Create a test settings file
        let settings_json = r#"{
            "env": {
                "TEST_VAR": "test_value",
                "CLAUDE_API_KEY": "test_api_key"
            }
        }"#;
        fs::write(&settings_path, settings_json).unwrap();

        // Load settings
        let settings = Settings::load_from_path(&settings_path).unwrap();

        // Check env vars
        assert_eq!(settings.env.get("TEST_VAR").unwrap(), "test_value");
        assert_eq!(settings.env.get("CLAUDE_API_KEY").unwrap(), "test_api_key");
    }

    #[test]
    fn settings_get_env_var() {
        // Create a temporary directory (use current dir to avoid TMPDIR issues in tarpaulin)
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            TempDir::new_in("tmp").unwrap()
        };
        let settings_path = temp_dir.path().join("settings.json");

        // Create a test settings file
        let settings_json = r#"{
            "env": {
                "TEST_VAR": "test_value",
                "CLAUDE_API_KEY": "test_api_key"
            }
        }"#;
        fs::write(&settings_path, settings_json).unwrap();

        // Load settings
        let settings = Settings::load_from_path(&settings_path).unwrap();

        // Set actual environment variable
        env::set_var("TEST_VAR_ENV", "env_value");

        // Test precedence - env var should take precedence
        env::set_var("TEST_VAR", "env_override");
        assert_eq!(settings.get_env_var("TEST_VAR").unwrap(), "env_override");

        // Test fallback to settings
        env::remove_var("TEST_VAR"); // Remove from environment
        assert_eq!(settings.get_env_var("TEST_VAR").unwrap(), "test_value");

        // Test actual env var
        assert_eq!(settings.get_env_var("TEST_VAR_ENV").unwrap(), "env_value");

        // Clean up
        env::remove_var("TEST_VAR_ENV");
    }

    // ── profile resolution (pure: MapEnv raw env, explicit active profile) ──

    #[test]
    fn resolve_no_profile_uses_base_env() {
        let settings = settings_with_profile();
        let raw = MapEnv::new();
        assert_eq!(
            settings
                .resolve_with(&raw, None, "ATLASSIAN_EMAIL")
                .as_deref(),
            Some("base@x.com")
        );
    }

    #[test]
    fn resolve_active_profile_uses_profile_env() {
        let settings = settings_with_profile();
        let raw = MapEnv::new();
        assert_eq!(
            settings
                .resolve_with(&raw, Some("work"), "ATLASSIAN_EMAIL")
                .as_deref(),
            Some("me@work.com")
        );
    }

    #[test]
    fn resolve_active_profile_does_not_consult_base() {
        // Isolated / AWS-faithful: a key present only in base is invisible while
        // a profile is active — fail loud rather than reuse a default token.
        let settings = settings_with_profile();
        let raw = MapEnv::new();
        assert_eq!(settings.resolve_with(&raw, Some("work"), "SHARED"), None);
    }

    #[test]
    fn resolve_process_env_wins_over_profile_and_base() {
        let settings = settings_with_profile();
        let raw = MapEnv::new().with("ATLASSIAN_EMAIL", "cli@x.com");
        assert_eq!(
            settings
                .resolve_with(&raw, Some("work"), "ATLASSIAN_EMAIL")
                .as_deref(),
            Some("cli@x.com")
        );
        assert_eq!(
            settings
                .resolve_with(&raw, None, "ATLASSIAN_EMAIL")
                .as_deref(),
            Some("cli@x.com")
        );
    }

    #[test]
    fn resolve_unknown_active_profile_yields_none() {
        // An unknown name never falls back to base; validation catches it at the
        // CLI boundary, but the resolver itself stays isolated.
        let settings = settings_with_profile();
        let raw = MapEnv::new();
        assert_eq!(
            settings.resolve_with(&raw, Some("nope"), "ATLASSIAN_EMAIL"),
            None
        );
    }

    #[test]
    fn active_profile_from_reads_and_trims_empty() {
        assert_eq!(active_profile_from(&MapEnv::new()), None);
        assert_eq!(
            active_profile_from(&MapEnv::new().with(PROFILE_ENV_VAR, "")),
            None
        );
        assert_eq!(
            active_profile_from(&MapEnv::new().with(PROFILE_ENV_VAR, "work")).as_deref(),
            Some("work")
        );
    }

    #[test]
    fn validate_profile_accepts_known() {
        assert!(settings_with_profile().validate_profile("work").is_ok());
    }

    #[test]
    fn validate_profile_rejects_unknown_and_lists_sorted() {
        let mut settings = settings_with_profile();
        settings
            .profiles
            .insert("personal".to_string(), Profile::default());
        let err = settings.validate_profile("wrok").unwrap_err().to_string();
        assert_eq!(
            err,
            "unknown profile 'wrok'; known profiles: personal, work"
        );
    }

    #[test]
    fn validate_profile_reports_none_when_empty() {
        let settings = Settings::default();
        let err = settings.validate_profile("work").unwrap_err().to_string();
        assert_eq!(err, "unknown profile 'work'; known profiles: (none)");
    }

    #[test]
    fn settings_parse_profiles_from_json() {
        let json = r#"{
            "env": { "BASE": "b" },
            "profiles": {
                "work": { "env": { "ATLASSIAN_EMAIL": "me@work.com" } }
            }
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.env.get("BASE").unwrap(), "b");
        assert_eq!(
            settings
                .profiles
                .get("work")
                .unwrap()
                .env
                .get("ATLASSIAN_EMAIL")
                .unwrap(),
            "me@work.com"
        );
    }

    #[test]
    fn settings_without_profiles_key_defaults_empty() {
        let settings: Settings = serde_json::from_str(r#"{ "env": {} }"#).unwrap();
        assert!(settings.profiles.is_empty());
    }

    // ── free get_env_var seam (pure: injected raw env + lazy settings loader) ──

    #[test]
    fn get_env_var_with_returns_raw_hit_without_loading() {
        let env = MapEnv::new().with("K", "v");
        let value = get_env_var_with(&env, || panic!("must not load settings"), "K").unwrap();
        assert_eq!(value, "v");
    }

    #[test]
    fn get_env_var_with_falls_back_to_base_settings() {
        let settings = settings_with_profile();
        let env = MapEnv::new();
        let value = get_env_var_with(&env, || Ok(settings), "ATLASSIAN_EMAIL").unwrap();
        assert_eq!(value, "base@x.com");
    }

    #[test]
    fn get_env_var_with_honours_active_profile() {
        let settings = settings_with_profile();
        let env = MapEnv::new().with(PROFILE_ENV_VAR, "work");
        let value = get_env_var_with(&env, || Ok(settings), "ATLASSIAN_EMAIL").unwrap();
        assert_eq!(value, "me@work.com");
    }

    #[test]
    fn get_env_var_with_missing_key_is_not_found() {
        let env = MapEnv::new();
        let err = get_env_var_with(&env, || Ok(Settings::default()), "MISSING")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Environment variable not found: MISSING"));
    }

    #[test]
    fn get_env_var_with_load_error_maps_to_not_found() {
        let env = MapEnv::new();
        let err =
            get_env_var_with(&env, || Err(anyhow::anyhow!("disk boom")), "MISSING").unwrap_err();
        // The load failure is the top-level context; the not-found error is its
        // source. The full chain (`{:#}`) carries both.
        assert_eq!(err.to_string(), "disk boom");
        let chain = format!("{err:#}");
        assert!(chain.contains("Environment variable not found: MISSING"));
    }

    // ── env-write helpers (injected paths, no HOME mutation — issue #1030) ──

    /// Creates a tempdir under `tmp/` (avoids TMPDIR issues in tarpaulin) and
    /// returns it with a `<dir>/.omni-dev/settings.json` path inside it.
    fn temp_settings_path() -> (TempDir, std::path::PathBuf) {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            TempDir::new_in("tmp").unwrap()
        };
        let path = temp_dir.path().join(".omni-dev").join("settings.json");
        (temp_dir, path)
    }

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn upsert_env_vars_creates_file_and_dir_with_secure_permissions() {
        let (_tmp, path) = temp_settings_path();

        Settings::upsert_env_vars(&path, &[("A_KEY", "a"), ("B_KEY", "b")]).unwrap();

        let val = read_json(&path);
        assert_eq!(val["env"]["A_KEY"], "a");
        assert_eq!(val["env"]["B_KEY"], "b");

        // Credential store hardening (issue #1128): dir 0700, file 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
            let file_mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(file_mode & 0o777, 0o600);
        }
    }

    #[test]
    fn upsert_env_vars_merges_and_preserves_unknown_fields() {
        let (_tmp, path) = temp_settings_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"env": {"OTHER_KEY": "keep_me"}, "extra": true}"#).unwrap();

        Settings::upsert_env_vars(&path, &[("A_KEY", "new")]).unwrap();

        let val = read_json(&path);
        assert_eq!(val["env"]["OTHER_KEY"], "keep_me");
        assert_eq!(val["extra"], true);
        assert_eq!(val["env"]["A_KEY"], "new");
    }

    #[test]
    fn upsert_env_vars_replaces_non_object_env() {
        let (_tmp, path) = temp_settings_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"env": "not-an-object"}"#).unwrap();

        Settings::upsert_env_vars(&path, &[("A_KEY", "a")]).unwrap();

        assert_eq!(read_json(&path)["env"]["A_KEY"], "a");
    }

    #[cfg(unix)]
    #[test]
    fn upsert_env_vars_retightens_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, path) = temp_settings_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"env": {}}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        Settings::upsert_env_vars(&path, &[("A_KEY", "a")]).unwrap();

        let file_mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
    }

    #[test]
    fn remove_env_vars_removes_listed_keys_and_preserves_rest() {
        let (_tmp, path) = temp_settings_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"env": {"A_KEY": "a", "B_KEY": "b", "OTHER_KEY": "keep"}, "extra": true}"#,
        )
        .unwrap();

        let removed = Settings::remove_env_vars(&path, &["A_KEY", "B_KEY", "ABSENT"]).unwrap();
        assert!(removed);

        let val = read_json(&path);
        assert!(val["env"].get("A_KEY").is_none());
        assert!(val["env"].get("B_KEY").is_none());
        assert_eq!(val["env"]["OTHER_KEY"], "keep");
        assert_eq!(val["extra"], true);
    }

    #[test]
    fn remove_env_vars_false_when_file_missing() {
        let (_tmp, path) = temp_settings_path();
        assert!(!Settings::remove_env_vars(&path, &["A_KEY"]).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn remove_env_vars_false_when_keys_absent_leaves_file_untouched() {
        let (_tmp, path) = temp_settings_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = r#"{"env": {"OTHER_KEY": "keep"}}"#;
        fs::write(&path, original).unwrap();

        let removed = Settings::remove_env_vars(&path, &["A_KEY"]).unwrap();
        assert!(!removed);
        // Not rewritten: the raw bytes are exactly as written.
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
}
