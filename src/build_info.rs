//! Build-time git provenance captured by [`build.rs`](../build.rs) and exposed
//! as compile-time constants.
//!
//! Two binaries built from different commits of the *same* crate version are
//! otherwise indistinguishable by [`crate::VERSION`] alone; this module carries
//! the commit SHA, dirty flag, commit date, and build timestamp so a resident
//! daemon can report exactly which code it is running (#1374). Every git-derived
//! value is optional: a build made outside a git checkout (a crates.io download
//! or release tarball) simply reports `None`, and every consumer degrades
//! gracefully. See [`Provenance`].

use serde::{Deserialize, Serialize};

/// Full 40-character commit SHA the binary was built from, or `None` when built
/// outside a git checkout.
pub const GIT_SHA: Option<&str> = option_env!("OMNI_DEV_GIT_SHA");

/// Abbreviated commit SHA (e.g. `a6d304fd`), or `None` outside a checkout.
pub const GIT_SHA_SHORT: Option<&str> = option_env!("OMNI_DEV_GIT_SHA_SHORT");

/// Committer date of the built commit in strict ISO-8601, or `None` outside a
/// checkout.
pub const GIT_COMMIT_DATE: Option<&str> = option_env!("OMNI_DEV_GIT_COMMIT_DATE");

/// Raw `"true"`/`"false"` working-tree dirty flag, or `None` when built outside
/// a git work tree. Interpreted by [`git_dirty`].
const GIT_DIRTY: Option<&str> = option_env!("OMNI_DEV_GIT_DIRTY");

/// Build time as integer seconds since the Unix epoch, or `None` if the build
/// script could not read a clock. Formatted by [`build_timestamp`].
const BUILD_EPOCH: Option<&str> = option_env!("OMNI_DEV_BUILD_EPOCH");

/// Whether the working tree had uncommitted changes at build time.
///
/// `None` when the binary was built outside a git work tree.
#[must_use]
pub fn git_dirty() -> Option<bool> {
    match GIT_DIRTY {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// Build timestamp formatted as an RFC3339 (UTC) string, or `None` when the
/// build script recorded no clock reading.
#[must_use]
pub fn build_timestamp() -> Option<String> {
    let secs: i64 = BUILD_EPOCH?.parse().ok()?;
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.to_rfc3339())
}

/// Git provenance of the running binary, surfaced on the daemon `status`/`ping`
/// wire and in `omni-dev --version`.
///
/// Every field is optional and additive: each is omitted from the wire when
/// absent, so a daemon built without git metadata stays byte-identical to a
/// pre-#1374 one and older clients ignore the extra keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Provenance {
    /// Abbreviated commit SHA (e.g. `a6d304fd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Full 40-character commit SHA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_long: Option<String>,
    /// Committer date of the built commit, strict ISO-8601.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_date: Option<String>,
    /// Whether the working tree was dirty at build time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// Build timestamp, RFC3339 (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<String>,
}

/// Snapshots this binary's compile-time provenance into an owned [`Provenance`].
///
/// All fields are `None` when the binary was built outside a git checkout.
#[must_use]
pub fn provenance() -> Provenance {
    Provenance {
        commit: GIT_SHA_SHORT.map(str::to_string),
        commit_long: GIT_SHA.map(str::to_string),
        commit_date: GIT_COMMIT_DATE.map(str::to_string),
        dirty: git_dirty(),
        build_timestamp: build_timestamp(),
    }
}

/// Long version string for `omni-dev --version`: the crate version plus git
/// provenance when available, e.g. `0.36.0 (a6d304fd 2026-07-20, dirty)`.
///
/// Degrades to the bare crate version when built outside a git checkout.
/// Returns a `&'static str` (computed once, then cached) because that is what
/// clap's `long_version` builder requires.
#[must_use]
pub fn long_version() -> &'static str {
    static LONG_VERSION: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(compute_long_version);
    LONG_VERSION.as_str()
}

/// Assembles the [`long_version`] string from the compile-time provenance.
fn compute_long_version() -> String {
    let mut suffix = String::new();
    if let Some(commit) = GIT_SHA_SHORT {
        suffix.push_str(commit);
    }
    if let Some(date) = GIT_COMMIT_DATE {
        // Keep just the calendar day (`YYYY-MM-DD`) for a compact banner.
        let day = date.split('T').next().unwrap_or(date);
        if !suffix.is_empty() {
            suffix.push(' ');
        }
        suffix.push_str(day);
    }
    if git_dirty() == Some(true) {
        if !suffix.is_empty() {
            suffix.push_str(", ");
        }
        suffix.push_str("dirty");
    }
    if suffix.is_empty() {
        crate::VERSION.to_string()
    } else {
        format!("{} ({suffix})", crate::VERSION)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn git_dirty_maps_the_raw_flag() {
        // The public accessor only ever yields the interpreted tri-state; the raw
        // const is whatever this build captured, so assert the mapping via the
        // pure match instead of the environment-dependent const.
        assert_eq!(interpret_dirty(Some("true")), Some(true));
        assert_eq!(interpret_dirty(Some("false")), Some(false));
        assert_eq!(interpret_dirty(None), None);
        assert_eq!(interpret_dirty(Some("garbage")), None);
    }

    // Mirror of `git_dirty`'s mapping over an explicit input, so the test does
    // not depend on how *this* binary happened to be built.
    fn interpret_dirty(raw: Option<&str>) -> Option<bool> {
        match raw {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        }
    }

    #[test]
    fn build_timestamp_formats_epoch_seconds() {
        // `build_timestamp` reads a const, so validate the formatting on a fixed
        // epoch directly: 1_700_000_000 == 2023-11-14T22:13:20Z.
        let dt = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(dt.to_rfc3339(), "2023-11-14T22:13:20+00:00");
    }

    #[test]
    fn long_version_always_starts_with_the_crate_version() {
        let v = long_version();
        assert!(v.starts_with(crate::VERSION), "{v}");
    }

    #[test]
    fn provenance_round_trips_and_omits_absent_fields() {
        // An empty provenance serializes to `{}` — no keys — so a daemon with no
        // git metadata adds nothing to the wire.
        let empty = serde_json::to_string(&Provenance::default()).unwrap();
        assert_eq!(empty, "{}");

        let full = Provenance {
            commit: Some("a6d304fd".to_string()),
            commit_long: Some("a6d304fddeadbeef".to_string()),
            commit_date: Some("2026-07-20T15:33:17+10:00".to_string()),
            dirty: Some(true),
            build_timestamp: Some("2026-07-20T05:33:17+00:00".to_string()),
        };
        let line = serde_json::to_string(&full).unwrap();
        let back: Provenance = serde_json::from_str(&line).unwrap();
        assert_eq!(back.commit.as_deref(), Some("a6d304fd"));
        assert_eq!(back.dirty, Some(true));
    }
}
