//! Message-id sharding for the local `.eml` archive.
//!
//! A 50k-message mailbox in one directory degrades badly on most
//! filesystems and makes `ls` unusable — two hex-ish chars off the Gmail id
//! gives ~256 buckets and even distribution for free.

use std::path::{Path, PathBuf};

/// Number of leading id characters used as the shard directory name.
const SHARD_PREFIX_LEN: usize = 2;

/// Builds `<archive_root>/messages/<shard>/<id>.eml`.
pub(crate) fn shard_path(archive_root: &Path, id: &str) -> PathBuf {
    archive_root
        .join("messages")
        .join(shard_key(id))
        .join(format!("{id}.eml"))
}

/// The shard directory name for `id`: its first [`SHARD_PREFIX_LEN`]
/// characters, lowercased. Real Gmail ids run 16+ hex characters, so the
/// defensive zero-pad below never actually triggers in practice.
fn shard_key(id: &str) -> String {
    let mut key: String = id
        .to_ascii_lowercase()
        .chars()
        .take(SHARD_PREFIX_LEN)
        .collect();
    while key.chars().count() < SHARD_PREFIX_LEN {
        key.push('0');
    }
    key
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn shard_path_places_eml_under_messages_shard_id() {
        let root = Path::new("/archive");
        let path = shard_path(root, "18d4f2a9b3c1e0aa");
        assert_eq!(
            path,
            PathBuf::from("/archive/messages/18/18d4f2a9b3c1e0aa.eml")
        );
    }

    #[test]
    fn shard_path_lowercases_the_shard_key() {
        let path = shard_path(Path::new("/archive"), "AB1234");
        assert_eq!(path, PathBuf::from("/archive/messages/ab/AB1234.eml"));
    }

    #[test]
    fn shard_path_is_deterministic() {
        let root = Path::new("/archive");
        assert_eq!(shard_path(root, "abc123"), shard_path(root, "abc123"));
    }

    #[test]
    fn shard_path_round_trips_parent_dir_name_to_shard_key() {
        let path = shard_path(Path::new("/archive"), "deadbeefcafe");
        assert_eq!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            shard_key("deadbeefcafe")
        );
    }

    #[test]
    fn shard_key_pads_short_ids_defensively() {
        assert_eq!(shard_key("a"), "a0");
        assert_eq!(shard_key(""), "00");
    }

    #[test]
    fn shard_keys_distribute_across_many_buckets() {
        // Realistic-looking hex ids sampled across the id space should
        // spread across a large fraction of the ~256 possible 2-char
        // buckets, not collapse onto a handful of them.
        let ids: Vec<String> = (0..5000u32)
            .map(|i| format!("{:016x}", u64::from(i).wrapping_mul(0x9E37_79B9_7F4A_7C15)))
            .collect();
        let buckets: HashSet<String> = ids.iter().map(|id| shard_key(id)).collect();
        assert!(
            buckets.len() > 200,
            "expected broad distribution across shard buckets, got {}",
            buckets.len()
        );
    }
}
