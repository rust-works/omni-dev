//! Automatic Chrome-profile resolution for `drive auth login` (mirrors
//! issue #1505/[ADR-0067](../../../docs/adrs/adr-0067.md), inherited by
//! [ADR-0069](../../../docs/adrs/adr-0069.md) "by field, not by
//! re-deciding it"): given an account's `email_address`, finds which local
//! Chrome profile is signed into it and builds a
//! [`crate::drive::auth::BrowserLaunch::Command`] that targets it.
//!
//! Opt-in per account via `chrome_profile_from_email`
//! (`crate::utils::settings::DriveAccountSettings`) — see
//! `super::auth::build_browser_config`, the sole caller. Chrome-only for
//! v1; the manual `browser_command` escape hatch already covers every other
//! browser.
//!
//! **Never guesses**: zero matches or more than one profile signed into the
//! same email both fail resolution rather than picking one.
//! **Always fails open**: Chrome not installed, `Local State` missing or
//! unparseable, or no unambiguous match all fall back to the caller's
//! default browser with a logged notice — never a hard login failure.
//!
//! Deliberately duplicated from (not shared with)
//! `crate::gmail::chrome_profile` — see that module's sibling and
//! [ADR-0069](../../../docs/adrs/adr-0069.md) §4: Drive is Gmail's second
//! consumer of this shape, one short of this repo's "extract only on a
//! third consumer" threshold, and this module has no `Gmail`-specific
//! coupling to rename either way.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The subset of Chrome's `Local State` JSON this module reads.
#[derive(Debug, Deserialize)]
struct LocalState {
    profile: ProfileSection,
}

#[derive(Debug, Deserialize)]
struct ProfileSection {
    #[serde(default)]
    info_cache: HashMap<String, ProfileInfo>,
}

#[derive(Debug, Deserialize)]
struct ProfileInfo {
    #[serde(default)]
    user_name: Option<String>,
}

/// The result of matching an email address against Chrome's installed
/// profiles.
#[derive(Debug, PartialEq, Eq)]
enum ProfileMatch {
    /// Exactly one profile directory is signed into the email.
    Found(String),
    /// No profile is signed into the email.
    NotFound,
    /// More than one profile is signed into the email — refuses to guess.
    Ambiguous(Vec<String>),
}

/// Matches `email` (case-insensitively, trimmed) against
/// `profile.info_cache.*.user_name` in `local_state_json`.
fn match_profiles(local_state_json: &str, email: &str) -> serde_json::Result<ProfileMatch> {
    let state: LocalState = serde_json::from_str(local_state_json)?;
    let email = email.trim();
    let mut matches: Vec<String> = state
        .profile
        .info_cache
        .into_iter()
        .filter(|(_, info)| {
            info.user_name
                .as_deref()
                .is_some_and(|user_name| user_name.trim().eq_ignore_ascii_case(email))
        })
        .map(|(dir, _)| dir)
        .collect();
    matches.sort();
    Ok(match matches.len() {
        0 => ProfileMatch::NotFound,
        1 => ProfileMatch::Found(matches.remove(0)),
        _ => ProfileMatch::Ambiguous(matches),
    })
}

/// The OS a Chrome-profile launch command is being built for — parameterized
/// (rather than a bare `cfg!` branch) so all three shapes are unit-testable
/// regardless of the host running the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChromeOs {
    Macos,
    Linux,
    Windows,
}

fn current_chrome_os() -> ChromeOs {
    if cfg!(target_os = "macos") {
        ChromeOs::Macos
    } else if cfg!(target_os = "windows") {
        ChromeOs::Windows
    } else {
        ChromeOs::Linux
    }
}

/// Builds the `{url}`-templated launch command for `profile_dir` on `os`,
/// consumed by [`crate::drive::auth::BrowserLaunch::Command`].
fn build_launch_command_for(os: ChromeOs, profile_dir: &str) -> Vec<String> {
    let profile_arg = format!("--profile-directory={profile_dir}");
    match os {
        ChromeOs::Macos => vec![
            "open".to_string(),
            "-na".to_string(),
            "Google Chrome".to_string(),
            "--args".to_string(),
            profile_arg,
            "{url}".to_string(),
        ],
        ChromeOs::Linux => vec![
            "google-chrome".to_string(),
            profile_arg,
            "{url}".to_string(),
        ],
        ChromeOs::Windows => vec!["chrome".to_string(), profile_arg, "{url}".to_string()],
    }
}

fn build_launch_command(profile_dir: &str) -> Vec<String> {
    build_launch_command_for(current_chrome_os(), profile_dir)
}

/// The default `Local State` file location for `os` — parameterized like
/// [`build_launch_command_for`] so all three shapes are unit-testable
/// regardless of the host running the tests. `None` when the OS's
/// user-data directory can't be determined (e.g. `$HOME` unset) — the
/// fail-open case that skips resolution entirely.
fn default_local_state_path_for(os: ChromeOs) -> Option<PathBuf> {
    match os {
        ChromeOs::Macos => {
            dirs::config_dir().map(|dir| dir.join("Google").join("Chrome").join("Local State"))
        }
        ChromeOs::Linux => {
            dirs::config_dir().map(|dir| dir.join("google-chrome").join("Local State"))
        }
        ChromeOs::Windows => dirs::data_local_dir().map(|dir| {
            dir.join("Google")
                .join("Chrome")
                .join("User Data")
                .join("Local State")
        }),
    }
}

/// [`default_local_state_path_for`] against the current host OS.
fn default_local_state_path() -> Option<PathBuf> {
    default_local_state_path_for(current_chrome_os())
}

/// Attempts to resolve a Chrome-profile launch command for `email` by
/// reading `local_state_path` — the injectable core, testable against a
/// fixture file without a real Chrome install. Fails open at every step:
/// a missing/unreadable file, unparseable JSON, or a zero/ambiguous match
/// all log a notice and return `None` rather than propagating an error.
fn resolve_launch_command_at(email: &str, local_state_path: &Path) -> Option<Vec<String>> {
    let content = match fs::read_to_string(local_state_path) {
        Ok(content) => content,
        Err(err) => {
            tracing::info!(
                "Chrome profile auto-resolution: could not read {} ({err}); \
                 falling back to the default browser",
                local_state_path.display()
            );
            return None;
        }
    };
    match match_profiles(&content, email) {
        Ok(ProfileMatch::Found(profile_dir)) => Some(build_launch_command(&profile_dir)),
        Ok(ProfileMatch::NotFound) => {
            tracing::info!(
                "Chrome profile auto-resolution: no installed Chrome profile is signed \
                 into {email}; falling back to the default browser"
            );
            None
        }
        Ok(ProfileMatch::Ambiguous(profile_dirs)) => {
            tracing::info!(
                "Chrome profile auto-resolution: multiple Chrome profiles are signed \
                 into {email} ({}); falling back to the default browser",
                profile_dirs.join(", ")
            );
            None
        }
        Err(err) => {
            tracing::info!(
                "Chrome profile auto-resolution: could not parse {} ({err}); \
                 falling back to the default browser",
                local_state_path.display()
            );
            None
        }
    }
}

/// [`resolve_launch_command_at`] against Chrome's real, per-OS `Local State`
/// path. `None` when that path can't even be determined — same fail-open
/// contract.
pub(crate) fn resolve_launch_command(email: &str) -> Option<Vec<String>> {
    let local_state_path = default_local_state_path()?;
    resolve_launch_command_at(email, &local_state_path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Thread-scoped log buffer, mirroring the `capture_info`/`CaptureWriter`
    /// pattern in `daemon/services/worktrees.rs`: `tracing`'s events only
    /// evaluate their field expressions (e.g. `path.display()`) when some
    /// subscriber is actually listening, so a test that never installs one
    /// leaves those expressions — and the coverage they'd otherwise add —
    /// unexercised even though the surrounding branch runs.
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

    /// Runs `f` under a thread-local INFO-level subscriber and returns
    /// everything it logged. `f` must be fully synchronous on this thread.
    fn capture_info(f: impl FnOnce()) -> String {
        let writer = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8_lossy(&writer.0.lock().unwrap()).into_owned();
        logs
    }

    const FIXTURE: &str = r#"{
        "profile": {
            "info_cache": {
                "Default": { "user_name": "alice@example.com", "name": "Alice" },
                "Profile 1": { "user_name": "bob@example.com", "name": "Bob" }
            }
        }
    }"#;

    #[test]
    fn match_profiles_finds_a_single_match() {
        assert_eq!(
            match_profiles(FIXTURE, "alice@example.com").unwrap(),
            ProfileMatch::Found("Default".to_string())
        );
    }

    #[test]
    fn match_profiles_is_case_insensitive_and_trims() {
        assert_eq!(
            match_profiles(FIXTURE, "  ALICE@Example.com  ").unwrap(),
            ProfileMatch::Found("Default".to_string())
        );
    }

    #[test]
    fn match_profiles_reports_no_match() {
        assert_eq!(
            match_profiles(FIXTURE, "nobody@example.com").unwrap(),
            ProfileMatch::NotFound
        );
    }

    #[test]
    fn match_profiles_reports_ambiguous_matches() {
        let fixture = r#"{
            "profile": {
                "info_cache": {
                    "Default": { "user_name": "shared@example.com" },
                    "Profile 1": { "user_name": "shared@example.com" }
                }
            }
        }"#;
        assert_eq!(
            match_profiles(fixture, "shared@example.com").unwrap(),
            ProfileMatch::Ambiguous(vec!["Default".to_string(), "Profile 1".to_string()])
        );
    }

    #[test]
    fn match_profiles_treats_missing_info_cache_as_no_match() {
        assert_eq!(
            match_profiles(r#"{ "profile": {} }"#, "alice@example.com").unwrap(),
            ProfileMatch::NotFound
        );
    }

    #[test]
    fn match_profiles_rejects_malformed_json() {
        assert!(match_profiles("not json", "alice@example.com").is_err());
    }

    #[test]
    fn match_profiles_rejects_json_missing_the_profile_key() {
        assert!(match_profiles(r#"{ "other": {} }"#, "alice@example.com").is_err());
    }

    #[test]
    fn build_launch_command_for_macos_targets_the_profile_with_open() {
        assert_eq!(
            build_launch_command_for(ChromeOs::Macos, "Profile 7"),
            vec![
                "open",
                "-na",
                "Google Chrome",
                "--args",
                "--profile-directory=Profile 7",
                "{url}",
            ]
        );
    }

    #[test]
    fn build_launch_command_for_linux_invokes_google_chrome_directly() {
        assert_eq!(
            build_launch_command_for(ChromeOs::Linux, "Profile 7"),
            vec!["google-chrome", "--profile-directory=Profile 7", "{url}"]
        );
    }

    #[test]
    fn build_launch_command_for_windows_invokes_chrome_directly() {
        assert_eq!(
            build_launch_command_for(ChromeOs::Windows, "Profile 7"),
            vec!["chrome", "--profile-directory=Profile 7", "{url}"]
        );
    }

    #[test]
    fn resolve_launch_command_at_finds_the_matching_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Local State");
        fs::write(&path, FIXTURE).unwrap();

        let command = resolve_launch_command_at("alice@example.com", &path).unwrap();
        assert!(command
            .iter()
            .any(|arg| arg == "--profile-directory=Default"));
    }

    #[test]
    fn resolve_launch_command_at_falls_open_on_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist");

        let mut result = None;
        let logs = capture_info(|| {
            result = Some(resolve_launch_command_at("alice@example.com", &path));
        });
        assert_eq!(result, Some(None));
        assert!(logs.contains("could not read"));
    }

    #[test]
    fn resolve_launch_command_at_falls_open_on_unparseable_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Local State");
        fs::write(&path, "not json").unwrap();

        let mut result = None;
        let logs = capture_info(|| {
            result = Some(resolve_launch_command_at("alice@example.com", &path));
        });
        assert_eq!(result, Some(None));
        assert!(logs.contains("could not parse"));
    }

    #[test]
    fn resolve_launch_command_at_falls_open_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Local State");
        fs::write(&path, FIXTURE).unwrap();

        assert_eq!(resolve_launch_command_at("nobody@example.com", &path), None);
    }

    #[test]
    fn resolve_launch_command_at_falls_open_on_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Local State");
        let fixture = r#"{
            "profile": {
                "info_cache": {
                    "Default": { "user_name": "shared@example.com" },
                    "Profile 1": { "user_name": "shared@example.com" }
                }
            }
        }"#;
        fs::write(&path, fixture).unwrap();

        let mut result = None;
        let logs = capture_info(|| {
            result = Some(resolve_launch_command_at("shared@example.com", &path));
        });
        assert_eq!(result, Some(None));
        assert!(logs.contains("multiple Chrome profiles"));
    }

    // ── default_local_state_path (per-OS paths) ─────────────────────────

    #[test]
    fn default_local_state_path_for_macos_targets_application_support() {
        let path =
            default_local_state_path_for(ChromeOs::Macos).expect("config_dir resolves in tests");
        assert!(path.ends_with("Google/Chrome/Local State"));
    }

    #[test]
    fn default_local_state_path_for_linux_targets_dot_config() {
        let path =
            default_local_state_path_for(ChromeOs::Linux).expect("config_dir resolves in tests");
        assert!(path.ends_with("google-chrome/Local State"));
    }

    #[test]
    fn default_local_state_path_for_windows_targets_local_app_data() {
        let path = default_local_state_path_for(ChromeOs::Windows)
            .expect("data_local_dir resolves in tests");
        assert!(path.ends_with("Google/Chrome/User Data/Local State"));
    }

    #[test]
    fn default_local_state_path_delegates_to_the_current_host_os() {
        assert_eq!(
            default_local_state_path(),
            default_local_state_path_for(current_chrome_os())
        );
    }

    // ── resolve_launch_command (public entry point) ─────────────────────

    #[test]
    fn resolve_launch_command_falls_open_for_an_unmatched_email() {
        // Exercises the real per-OS `Local State` path without depending on
        // (or reading) any real Chrome profile data on the host: no
        // installed profile is plausibly signed into this email, so every
        // fail-open branch (missing/unreadable file, no match) converges on
        // `None` regardless of the host running the test.
        assert_eq!(
            resolve_launch_command("definitely-not-a-real-account+omni-dev-test@invalid.example"),
            None
        );
    }
}
