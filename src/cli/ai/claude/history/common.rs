//! Shared helpers and types for the `history` subcommands.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::Serialize;

/// Output format shared by `history sync` (and any future siblings).
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Human-readable lines, one per action.
    #[default]
    Text,
    /// Machine-readable YAML document.
    Yaml,
}

/// Returns the default source root: `$HOME/.claude/projects`.
pub fn default_source_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to determine home directory")?;
    Ok(home.join(".claude").join("projects"))
}

/// Decodes a Claude project slug (e.g. `-Users-jky-tmp`) to its original
/// filesystem path (`/Users/jky/tmp`).
///
/// Claude Code encodes the absolute cwd by replacing each path separator with
/// `-`. The transformation isn't fully invertible — a literal `-` in a
/// directory name becomes ambiguous — but the reverse mapping is good enough
/// for `--project` matching against a user-supplied path.
pub fn decode_slug(slug: &str) -> String {
    slug.replace('-', "/")
}

/// Returns true if `candidate` is `root` itself or a descendant of it.
///
/// Canonicalises both paths so platform-level path aliasing (e.g. macOS's
/// `/tmp` → `/private/tmp`) doesn't produce false negatives. `candidate` need
/// not exist yet — we canonicalise the deepest existing ancestor and append
/// the remainder.
pub fn is_inside(candidate: &Path, root: &Path) -> bool {
    let candidate = canonicalise_with_walkup(candidate);
    let root = canonicalise_with_walkup(root);
    candidate.starts_with(&root)
}

fn canonicalise_with_walkup(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    // Walk up to the deepest ancestor that exists, canonicalise it, then
    // append the trailing components verbatim.
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    loop {
        if cur.exists() {
            if let Ok(canon) = std::fs::canonicalize(cur) {
                let mut out = canon;
                for c in tail.iter().rev() {
                    out.push(c);
                }
                return out;
            }
            break;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent;
            }
            _ => break,
        }
    }
    p.to_path_buf()
}

/// Parses a `--since` value: relative duration shorthand (`30s`, `5m`, `2h`,
/// `7d`, `4w`) or an RFC 3339 timestamp. Returns the `DateTime<Utc>` cutoff:
/// sessions whose source mtime is **at or after** this cutoff are included.
pub fn parse_since(spec: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("--since value is empty");
    }
    if let Some(seconds) = parse_relative_seconds(spec) {
        let secs = i64::try_from(seconds)
            .with_context(|| format!("--since duration {spec} is out of range"))?;
        let delta = chrono::Duration::seconds(secs);
        return now
            .checked_sub_signed(delta)
            .with_context(|| format!("--since {spec} underflows the calendar"));
    }
    if let Ok(ts) = DateTime::parse_from_rfc3339(spec) {
        return Ok(ts.with_timezone(&Utc));
    }
    anyhow::bail!(
        "--since must be a relative duration like `7d` or an RFC3339 timestamp; got `{spec}`"
    )
}

fn parse_relative_seconds(spec: &str) -> Option<u64> {
    let unit_byte = *spec.as_bytes().last()?;
    let unit_seconds: u64 = match unit_byte {
        b's' => 1,
        b'm' => 60,
        b'h' => 60 * 60,
        b'd' => 24 * 60 * 60,
        b'w' => 7 * 24 * 60 * 60,
        _ => return None,
    };
    let digits = &spec[..spec.len() - 1];
    if digits.is_empty() {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(unit_seconds)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()
    }

    #[test]
    fn decode_slug_replaces_dashes_with_slashes() {
        assert_eq!(decode_slug("-Users-jky-tmp"), "/Users/jky/tmp");
    }

    #[test]
    fn decode_slug_handles_bare_string() {
        assert_eq!(decode_slug("foo"), "foo");
    }

    #[test]
    fn since_seconds() {
        let cutoff = parse_since("30s", fixed_now()).unwrap();
        assert_eq!(cutoff.timestamp(), fixed_now().timestamp() - 30);
    }

    #[test]
    fn since_minutes() {
        let cutoff = parse_since("15m", fixed_now()).unwrap();
        assert_eq!(cutoff.timestamp(), fixed_now().timestamp() - 15 * 60);
    }

    #[test]
    fn since_hours() {
        let cutoff = parse_since("2h", fixed_now()).unwrap();
        assert_eq!(cutoff.timestamp(), fixed_now().timestamp() - 2 * 60 * 60);
    }

    #[test]
    fn since_days() {
        let cutoff = parse_since("7d", fixed_now()).unwrap();
        assert_eq!(
            cutoff.timestamp(),
            fixed_now().timestamp() - 7 * 24 * 60 * 60
        );
    }

    #[test]
    fn since_weeks() {
        let cutoff = parse_since("2w", fixed_now()).unwrap();
        assert_eq!(
            cutoff.timestamp(),
            fixed_now().timestamp() - 14 * 24 * 60 * 60
        );
    }

    #[test]
    fn since_rfc3339() {
        let cutoff = parse_since("2026-05-01T11:00:00Z", fixed_now()).unwrap();
        assert_eq!(cutoff.timestamp(), fixed_now().timestamp() - 60 * 60);
    }

    #[test]
    fn since_rfc3339_with_offset() {
        let cutoff = parse_since("2026-05-01T13:00:00+01:00", fixed_now()).unwrap();
        assert_eq!(cutoff.timestamp(), fixed_now().timestamp());
    }

    #[test]
    fn since_rejects_empty() {
        assert!(parse_since("", fixed_now()).is_err());
        assert!(parse_since("   ", fixed_now()).is_err());
    }

    #[test]
    fn since_rejects_unknown_unit() {
        assert!(parse_since("5y", fixed_now()).is_err());
    }

    #[test]
    fn since_rejects_unit_without_digits() {
        assert!(parse_since("d", fixed_now()).is_err());
    }

    #[test]
    fn since_rejects_garbage() {
        assert!(parse_since("not-a-duration", fixed_now()).is_err());
    }

    #[test]
    fn is_inside_detects_descendant() {
        let parent = std::env::temp_dir();
        assert!(is_inside(&parent, &parent));
        let child = parent.join("does-not-need-to-exist");
        assert!(is_inside(&child, &parent));
    }

    #[test]
    fn is_inside_rejects_unrelated_paths() {
        let a = PathBuf::from("/tmp/aaa-history-a");
        let b = PathBuf::from("/tmp/aaa-history-b");
        assert!(!is_inside(&a, &b));
    }
}
