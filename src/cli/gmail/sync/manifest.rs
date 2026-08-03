//! `manifest.jsonl`: the only record of Gmail-side metadata for an archived
//! message.
//!
//! Gmail labels are not part of the RFC 2822 body, so a `labelsAdded`
//! history event cannot be represented by rewriting the `.eml` — label
//! changes update the manifest record only. This is *not* a derived index
//! that could be regenerated from the `.eml` files; it is the sole record
//! of label state, so (unlike `state.rs`'s deliberately disposable
//! `state.json`) a corrupt manifest is a hard failure, never a silent
//! start-from-empty that would orphan every message's label history.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One archived message's Gmail-side metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ManifestRecord {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) label_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) internal_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rfc822_msgid: Option<String>,
    /// Path to the `.eml` file, relative to the archive root.
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    /// The watermark this record was last touched at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) history_id: Option<String>,
    /// Soft-delete marker — set when the message no longer appears in a
    /// full listing. The `.eml` is never removed; a later listing showing
    /// the id again clears this via [`Manifest::undelete`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deleted_at: Option<DateTime<Utc>>,
}

/// The whole manifest, keyed by message id (a `BTreeMap` for deterministic,
/// greppable output order).
///
/// Loaded entirely into memory, mutated, and rewritten wholesale on
/// [`Self::save`] — not appended-then-compacted. An append-only log can't
/// represent "update this existing record's labels" without a
/// last-write-wins reconstruction on every read; at realistic scale (50k
/// messages × ~250 bytes ≈ 12 MB) loading the whole thing is trivial. The
/// file stays one-JSON-object-per-line; only the *write* strategy is
/// "recompute and replace".
#[derive(Debug, Default)]
pub(crate) struct Manifest(BTreeMap<String, ManifestRecord>);

impl Manifest {
    /// Loads `manifest.jsonl`. An absent file is an empty manifest (first
    /// run); a present-but-unparseable file is a hard error — see the
    /// module doc for why this differs from `state.rs::load`.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read manifest at {}", path.display()))
            }
        };
        let mut map = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: ManifestRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "Failed to parse manifest line {} at {} — the manifest is the only record \
                     of this archive's label state and is never silently discarded; fix or \
                     restore it from a backup before re-running sync",
                    i + 1,
                    path.display()
                )
            })?;
            map.insert(record.id.clone(), record);
        }
        Ok(Self(map))
    }

    pub(crate) fn get(&self, id: &str) -> Option<&ManifestRecord> {
        self.0.get(id)
    }

    pub(crate) fn upsert(&mut self, record: ManifestRecord) {
        self.0.insert(record.id.clone(), record);
    }

    /// Adds labels to an existing record. A no-op if `id` isn't in the
    /// manifest yet (e.g. its own `messagesAdded` event hasn't been
    /// processed within this run) rather than an error — labels-only
    /// history events never fetch or create a message on their own.
    pub(crate) fn add_labels(&mut self, id: &str, label_ids: &[String]) {
        if let Some(record) = self.0.get_mut(id) {
            for label in label_ids {
                if !record.label_ids.contains(label) {
                    record.label_ids.push(label.clone());
                }
            }
        }
    }

    /// Removes labels from an existing record. A no-op if `id` is absent.
    pub(crate) fn remove_labels(&mut self, id: &str, label_ids: &[String]) {
        if let Some(record) = self.0.get_mut(id) {
            record.label_ids.retain(|l| !label_ids.contains(l));
        }
    }

    /// Soft-deletes a record: the `.eml` is never touched, only
    /// `deleted_at` is set (a no-op if `id` is absent or already deleted).
    pub(crate) fn mark_deleted(&mut self, id: &str, at: DateTime<Utc>) {
        if let Some(record) = self.0.get_mut(id) {
            record.deleted_at = Some(at);
        }
    }

    /// Clears a previously-set soft-delete marker (a no-op if `id` is
    /// absent or not currently deleted).
    pub(crate) fn undelete(&mut self, id: &str) {
        if let Some(record) = self.0.get_mut(id) {
            record.deleted_at = None;
        }
    }

    /// Ids of every record not currently soft-deleted.
    pub(crate) fn ids_not_deleted(&self) -> impl Iterator<Item = &str> {
        self.0
            .values()
            .filter(|r| r.deleted_at.is_none())
            .map(|r| r.id.as_str())
    }

    /// Atomically rewrites `manifest.jsonl` in full (temp file in the same
    /// directory + rename via the `tempfile` crate, which additionally
    /// cleans up the temp file if `persist` isn't reached — the same
    /// pattern `src/cli/ai/claude/history/sync.rs` uses for its own atomic
    /// writes).
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        let dir = dir.unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create a temp file in {}", dir.display()))?;
        for record in self.0.values() {
            serde_json::to_writer(&mut tmp, record)
                .context("Failed to serialise a manifest record")?;
            tmp.write_all(b"\n")
                .context("Failed to write a manifest record")?;
        }
        tmp.flush().context("Failed to flush the manifest")?;
        tmp.persist(path)
            .map_err(|e| e.error)
            .with_context(|| format!("Failed to publish manifest to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_record(id: &str) -> ManifestRecord {
        ManifestRecord {
            id: id.to_string(),
            thread_id: Some("t1".to_string()),
            label_ids: vec!["INBOX".to_string()],
            internal_date: Some("1700000000000".to_string()),
            subject: Some("Hello".to_string()),
            from: Some("a@example.com".to_string()),
            rfc822_msgid: Some("<1@example.com>".to_string()),
            path: PathBuf::from(format!("messages/{}/{}.eml", &id[..2.min(id.len())], id)),
            size: 42,
            history_id: Some("1000".to_string()),
            deleted_at: None,
        }
    }

    // ── load ─────────────────────────────────────────────────────────

    #[test]
    fn load_absent_file_is_an_empty_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Manifest::load(&dir.path().join("manifest.jsonl")).unwrap();
        assert!(manifest.get("m1").is_none());
    }

    #[test]
    fn load_rejects_a_corrupt_line_instead_of_starting_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.jsonl");
        std::fs::write(&path, "{\"id\": \"m1\"\nnot json at all\n").unwrap();
        let err = Manifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("Failed to parse manifest line"));
    }

    #[test]
    fn load_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.jsonl");
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        manifest.save(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("\n{raw}\n")).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert!(loaded.get("m1").is_some());
    }

    // ── round-trip / upsert ────────────────────────────────────────────

    #[test]
    fn save_then_load_round_trips_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.jsonl");
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        manifest.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.get("m1"), Some(&sample_record("m1")));
    }

    #[test]
    fn save_writes_one_compact_json_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.jsonl");
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        manifest.upsert(sample_record("m2"));
        manifest.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.jsonl");
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        manifest.save(&path).unwrap();

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path() != path)
            .collect();
        assert!(leftover.is_empty(), "expected no leftover temp files");
    }

    // ── labels ───────────────────────────────────────────────────────

    #[test]
    fn add_labels_appends_without_duplicating() {
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        manifest.add_labels("m1", &["IMPORTANT".to_string(), "INBOX".to_string()]);
        assert_eq!(
            manifest.get("m1").unwrap().label_ids,
            vec!["INBOX".to_string(), "IMPORTANT".to_string()]
        );
    }

    #[test]
    fn add_labels_is_a_no_op_when_id_is_absent() {
        let mut manifest = Manifest::default();
        manifest.add_labels("missing", &["INBOX".to_string()]);
        assert!(manifest.get("missing").is_none());
    }

    #[test]
    fn remove_labels_drops_only_the_named_labels() {
        let mut manifest = Manifest::default();
        let mut record = sample_record("m1");
        record.label_ids = vec!["INBOX".to_string(), "UNREAD".to_string()];
        manifest.upsert(record);
        manifest.remove_labels("m1", &["UNREAD".to_string()]);
        assert_eq!(
            manifest.get("m1").unwrap().label_ids,
            vec!["INBOX".to_string()]
        );
    }

    // ── soft-delete ────────────────────────────────────────────────────

    #[test]
    fn mark_deleted_sets_deleted_at_and_undelete_clears_it() {
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        let now = Utc::now();
        manifest.mark_deleted("m1", now);
        assert_eq!(manifest.get("m1").unwrap().deleted_at, Some(now));

        manifest.undelete("m1");
        assert_eq!(manifest.get("m1").unwrap().deleted_at, None);
    }

    #[test]
    fn ids_not_deleted_excludes_soft_deleted_records() {
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        manifest.upsert(sample_record("m2"));
        manifest.mark_deleted("m1", Utc::now());

        let remaining: Vec<&str> = manifest.ids_not_deleted().collect();
        assert_eq!(remaining, vec!["m2"]);
    }

    #[test]
    fn label_only_change_never_touches_the_eml_path_or_size() {
        let mut manifest = Manifest::default();
        manifest.upsert(sample_record("m1"));
        let before = manifest.get("m1").unwrap().clone();
        manifest.add_labels("m1", &["IMPORTANT".to_string()]);
        let after = manifest.get("m1").unwrap();
        assert_eq!(before.path, after.path);
        assert_eq!(before.size, after.size);
    }
}
