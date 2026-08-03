//! Message-date sharding for the local `.eml` archive.
//!
//! Sharded by the date Gmail recorded for the message (`internal_date`), not
//! by any part of the message id. An earlier id-prefix scheme was measured
//! against 5,824 real ids from a single label and found not to distribute at
//! all: Gmail ids are time-ordered hex, so a fixed-length prefix only ever
//! populated 10 of its 256 possible buckets, the largest holding 33% of the
//! archive — and it gets worse over time, since new mail always lands in the
//! newest prefix while older buckets stay frozen. Sharding by
//! `messages/<year>/<month>/<day>/<id>.eml` instead, measured on the same
//! corpus, gives 2,555 day-directories with the largest holding 154 files —
//! and, unlike an id prefix, makes the archive glob-able by date range, which
//! is the common query for the auditing use case this archive exists for.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};

/// Directory name used when a message has no parseable `internal_date`.
/// Rare in practice — Gmail populates it regardless of the requested
/// `format` — but the archive still needs somewhere deterministic to put
/// such a message rather than erroring or guessing a date.
const UNKNOWN_DATE_BUCKET: &str = "unknown-date";

/// Builds `<archive_root>/messages/<year>/<month>/<day>/<id>.eml`, or
/// `<archive_root>/messages/unknown-date/<id>.eml` when `date` is `None`.
pub(crate) fn shard_path(archive_root: &Path, id: &str, date: Option<DateTime<Utc>>) -> PathBuf {
    let mut path = archive_root.join("messages");
    match date {
        Some(date) => {
            path.push(format!("{:04}", date.year()));
            path.push(format!("{:02}", date.month()));
            path.push(format!("{:02}", date.day()));
        }
        None => path.push(UNKNOWN_DATE_BUCKET),
    }
    path.push(format!("{id}.eml"));
    path
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn date(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    #[test]
    fn shard_path_places_eml_under_year_month_day() {
        let path = shard_path(Path::new("/archive"), "abc123", Some(date(2026, 3, 5)));
        assert_eq!(
            path,
            PathBuf::from("/archive/messages/2026/03/05/abc123.eml")
        );
    }

    #[test]
    fn shard_path_zero_pads_month_and_day() {
        let path = shard_path(Path::new("/archive"), "id1", Some(date(2026, 1, 2)));
        assert_eq!(path, PathBuf::from("/archive/messages/2026/01/02/id1.eml"));
    }

    #[test]
    fn shard_path_falls_back_to_unknown_date_bucket() {
        let path = shard_path(Path::new("/archive"), "id1", None);
        assert_eq!(
            path,
            PathBuf::from("/archive/messages/unknown-date/id1.eml")
        );
    }

    #[test]
    fn shard_path_is_deterministic() {
        let root = Path::new("/archive");
        let d = Some(date(2026, 6, 1));
        assert_eq!(shard_path(root, "x", d), shard_path(root, "x", d));
    }

    #[test]
    fn shard_path_groups_same_day_messages_into_one_directory() {
        let root = Path::new("/archive");
        let d = Some(date(2026, 6, 1));
        let a = shard_path(root, "id-a", d);
        let b = shard_path(root, "id-b", d);
        assert_eq!(a.parent(), b.parent());
    }

    #[test]
    fn shard_path_separates_different_days() {
        let root = Path::new("/archive");
        let a = shard_path(root, "id", Some(date(2026, 6, 1)));
        let b = shard_path(root, "id", Some(date(2026, 6, 2)));
        assert_ne!(a.parent(), b.parent());
    }
}
