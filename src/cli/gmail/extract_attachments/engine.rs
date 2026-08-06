//! The algorithm behind `gmail extract-attachments`.
//!
//! Retroactively extracts attachments for messages already archived by
//! `gmail sync`/`sync-all`, without any network call — the local-only
//! counterpart to `sync/engine.rs`'s `fetch_and_write_one` extraction step,
//! run against on-disk bytes instead of freshly-fetched ones. See
//! ADR-0065's "No retroactive backfill, for free" consequence, which this
//! closes (#1510).

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::gmail::sync::engine::{manifest_path, write_attachments_atomically};
use crate::cli::gmail::sync::manifest::Manifest;
use crate::cli::gmail::sync::shard::attachments_dir;
use crate::gmail::attachments::extract_attachments;

use super::report::{ExtractAction, ExtractAttachmentsReport, ExtractError};

/// Options for [`run_extract_attachments`].
pub(crate) struct ExtractAttachmentsOptions {
    pub(crate) archive_dir: PathBuf,
    pub(crate) dry_run: bool,
}

/// Walks every non-deleted manifest record with a plausible attachment
/// (`attachment_count > 0` — the same cheap heuristic scan `gmail sync`
/// always runs regardless of `--extract-attachments`, see ADR-0065
/// Decision 4) and, for any not already extracted, parses its archived
/// `.eml` and writes out its attachments.
///
/// Per-record failures accumulate into `report.errors` and never abort the
/// batch, mirroring `sync/engine.rs`'s fetch loop. Performs no
/// stdout/stderr I/O itself — the CLI layer renders the report and only
/// then decides the process exit code (ADR-0064 Decision 4).
pub(crate) fn run_extract_attachments(
    opts: &ExtractAttachmentsOptions,
) -> Result<ExtractAttachmentsReport> {
    let manifest = Manifest::load(&manifest_path(&opts.archive_dir))?;
    let mut report = ExtractAttachmentsReport::default();

    for record in manifest.records_not_deleted() {
        if record.attachment_count == 0 {
            continue;
        }

        let dir = attachments_dir(&opts.archive_dir, &record.id, record.internal_date_utc());
        if dir.exists() {
            // Already extracted by an earlier run of this command (or by
            // `gmail sync --extract-attachments` when first fetched) — the
            // same presence-on-disk idempotence `sync` itself relies on.
            // Trustworthy even after a prior failed attempt:
            // `write_attachments_atomically` only ever leaves `dir` behind
            // fully populated, never partial, so "exists" always means
            // "done". Silent, like `sync`'s own already-archived skip.
            continue;
        }

        let eml_path = opts.archive_dir.join(&record.path);
        let bytes = match std::fs::read(&eml_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                report.errors.push(ExtractError {
                    id: record.id.clone(),
                    reason: format!("Failed to read {}: {e}", eml_path.display()),
                });
                continue;
            }
        };

        let extracted = extract_attachments(&bytes);
        if extracted.is_empty() {
            // The heuristic and the real parser can disagree (ADR-0065) —
            // nothing to write, not an error.
            continue;
        }

        if opts.dry_run {
            report.actions.push(ExtractAction::WouldExtract {
                id: record.id.clone(),
                count: extracted.len(),
            });
            continue;
        }

        match write_attachments_atomically(&dir, &extracted) {
            Ok(()) => report.actions.push(ExtractAction::Extracted {
                id: record.id.clone(),
                count: extracted.len(),
            }),
            Err(e) => report.errors.push(ExtractError {
                id: record.id.clone(),
                reason: format!("{e:#}"),
            }),
        }
    }

    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::{Path, PathBuf};

    use base64::Engine as _;
    use chrono::{DateTime, Utc};

    use crate::cli::gmail::sync::manifest::ManifestRecord;
    use crate::cli::gmail::sync::shard::shard_path;

    use super::*;

    const INTERNAL_DATE_MS: i64 = 1_700_000_000_000;

    fn internal_date() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(INTERNAL_DATE_MS).unwrap()
    }

    /// A multipart message with one base64-encoded `application/pdf`
    /// attachment — mirrors `sync.rs`'s `multipart_with_base64_attachment`
    /// fixture.
    fn multipart_with_base64_attachment() -> String {
        let encoded_attachment = base64::engine::general_purpose::STANDARD.encode(b"PDF-CONTENT");
        format!(
            "Subject: Report\r\n\
From: a@example.com\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: text/plain\r\n\
\r\n\
Hello\r\n\
--BOUNDARY\r\n\
Content-Type: application/pdf\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
\r\n\
{encoded_attachment}\r\n\
--BOUNDARY--\r\n"
        )
    }

    fn plain_message_without_attachment() -> String {
        "Subject: Hi\r\nFrom: a@example.com\r\n\r\nJust text.\r\n".to_string()
    }

    fn sample_record(id: &str, attachment_count: u32, path: PathBuf) -> ManifestRecord {
        ManifestRecord {
            id: id.to_string(),
            thread_id: None,
            label_ids: Vec::new(),
            internal_date: Some(INTERNAL_DATE_MS.to_string()),
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: None,
            in_reply_to: None,
            references: None,
            attachment_count,
            attachment_filenames: Vec::new(),
            path,
            size: 0,
            history_id: None,
            deleted_at: None,
        }
    }

    /// Writes `contents` at the same shard path a real sync would have
    /// used for `id`, and returns the manifest record pointing at it.
    fn archive_message(
        archive_dir: &Path,
        id: &str,
        attachment_count: u32,
        contents: &str,
    ) -> ManifestRecord {
        let path = shard_path(archive_dir, id, Some(internal_date()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        let relative = path.strip_prefix(archive_dir).unwrap().to_path_buf();
        sample_record(id, attachment_count, relative)
    }

    fn save_manifest(archive_dir: &Path, records: Vec<ManifestRecord>) {
        let mut manifest = Manifest::default();
        for record in records {
            manifest.upsert(record);
        }
        manifest.save(&manifest_path(archive_dir)).unwrap();
    }

    #[test]
    fn extracts_attachments_for_a_candidate_message() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        let record = archive_message(&archive_dir, "m1", 1, &multipart_with_base64_attachment());
        save_manifest(&archive_dir, vec![record]);

        let report = run_extract_attachments(&ExtractAttachmentsOptions {
            archive_dir: archive_dir.clone(),
            dry_run: false,
        })
        .unwrap();

        assert_eq!(report.errors.len(), 0);
        assert_eq!(report.actions.len(), 1);
        assert!(matches!(
            &report.actions[0],
            ExtractAction::Extracted { id, count } if id == "m1" && *count == 1
        ));

        let written = attachments_dir(&archive_dir, "m1", Some(internal_date())).join("report.pdf");
        assert_eq!(std::fs::read(written).unwrap(), b"PDF-CONTENT");
    }

    #[test]
    fn skips_a_record_with_zero_attachment_count_without_reading_its_eml() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        // attachment_count 0 despite the body actually having one — proves
        // the fast-path filter never even opens the file.
        let record = archive_message(&archive_dir, "m1", 0, &multipart_with_base64_attachment());
        save_manifest(&archive_dir, vec![record]);

        let report = run_extract_attachments(&ExtractAttachmentsOptions {
            archive_dir: archive_dir.clone(),
            dry_run: false,
        })
        .unwrap();

        assert!(report.actions.is_empty());
        assert!(report.errors.is_empty());
        assert!(!attachments_dir(&archive_dir, "m1", Some(internal_date())).exists());
    }

    #[test]
    fn skips_a_message_already_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        let record = archive_message(&archive_dir, "m1", 1, &multipart_with_base64_attachment());
        save_manifest(&archive_dir, vec![record]);

        let existing_dir = attachments_dir(&archive_dir, "m1", Some(internal_date()));
        std::fs::create_dir_all(&existing_dir).unwrap();
        std::fs::write(existing_dir.join("already-there.txt"), b"x").unwrap();

        let report = run_extract_attachments(&ExtractAttachmentsOptions {
            archive_dir,
            dry_run: false,
        })
        .unwrap();

        assert!(report.actions.is_empty());
        assert!(report.errors.is_empty());
        // Untouched — proves it was skipped, not re-extracted.
        assert!(!existing_dir.join("report.pdf").exists());
    }

    #[test]
    fn skips_soft_deleted_records_even_with_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        let mut record =
            archive_message(&archive_dir, "m1", 1, &multipart_with_base64_attachment());
        record.deleted_at = Some(Utc::now());
        save_manifest(&archive_dir, vec![record]);

        let report = run_extract_attachments(&ExtractAttachmentsOptions {
            archive_dir,
            dry_run: false,
        })
        .unwrap();

        assert!(report.actions.is_empty());
        assert!(report.errors.is_empty());
    }

    #[test]
    fn dry_run_reports_an_accurate_count_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        let record = archive_message(&archive_dir, "m1", 1, &multipart_with_base64_attachment());
        save_manifest(&archive_dir, vec![record]);

        let report = run_extract_attachments(&ExtractAttachmentsOptions {
            archive_dir: archive_dir.clone(),
            dry_run: true,
        })
        .unwrap();

        assert!(matches!(
            &report.actions[0],
            ExtractAction::WouldExtract { id, count } if id == "m1" && *count == 1
        ));
        assert!(!attachments_dir(&archive_dir, "m1", Some(internal_date())).exists());
    }

    #[test]
    fn a_missing_eml_accumulates_an_error_and_the_run_continues() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        // Manifest claims a candidate message but its `.eml` was never
        // written — a corrupt-archive edge case, not exercised by `sync`
        // itself since it always writes both together.
        let missing = sample_record("m1", 1, PathBuf::from("messages/missing/m1.eml"));
        let present = archive_message(&archive_dir, "m2", 1, &multipart_with_base64_attachment());
        save_manifest(&archive_dir, vec![missing, present]);

        let report = run_extract_attachments(&ExtractAttachmentsOptions {
            archive_dir,
            dry_run: false,
        })
        .unwrap();

        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].id, "m1");
        // m2 still got processed despite m1's failure.
        assert_eq!(report.actions.len(), 1);
        assert!(matches!(&report.actions[0], ExtractAction::Extracted { id, .. } if id == "m2"));
    }

    #[test]
    fn a_message_with_no_real_attachments_is_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        // attachment_count says 1 (heuristic), but the body has none — the
        // ADR-0065-documented disagreement case.
        let record = archive_message(&archive_dir, "m1", 1, &plain_message_without_attachment());
        save_manifest(&archive_dir, vec![record]);

        let report = run_extract_attachments(&ExtractAttachmentsOptions {
            archive_dir,
            dry_run: false,
        })
        .unwrap();

        assert!(report.actions.is_empty());
        assert!(report.errors.is_empty());
    }
}
