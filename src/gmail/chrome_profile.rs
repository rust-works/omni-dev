//! Automatic Chrome-profile resolution for `gmail auth login` (issue #1505,
//! [ADR-0067](../../../docs/adrs/adr-0067.md)): given an account's
//! `email_address`, finds which local Chrome profile is signed into it and
//! builds a [`crate::gmail::auth::BrowserLaunch::Command`] that targets it.
//!
//! Opt-in per account via `chrome_profile_from_email`
//! (`crate::utils::settings::GmailAccountSettings`) — see
//! `super::auth::build_browser_config`, the sole caller. Chrome-only for
//! v1; the manual `browser_command` escape hatch already covers every other
//! browser.
//!
//! **Never guesses**: zero matches or more than one profile signed into the
//! same email both fail resolution rather than picking one.
//! **Always fails open**: Chrome not installed, `Local State` missing or
//! unparseable, or no unambiguous match all fall back to the caller's
//! default browser with a logged notice — never a hard login failure.

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
/// consumed by [`crate::gmail::auth::BrowserLaunch::Command`].
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

/// The default per-OS location of Chrome's `Local State` file, or `None`
/// when the OS's user-data directory can't be determined (e.g. `$HOME`
/// unset) — the fail-open case that skips resolution entirely.
fn default_local_state_path() -> Option<PathBuf> {
    match current_chrome_os() {
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

        assert_eq!(resolve_launch_command_at("alice@example.com", &path), None);
    }

    #[test]
    fn resolve_launch_command_at_falls_open_on_unparseable_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Local State");
        fs::write(&path, "not json").unwrap();

        assert_eq!(resolve_launch_command_at("alice@example.com", &path), None);
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

        assert_eq!(resolve_launch_command_at("shared@example.com", &path), None);
    }
}
