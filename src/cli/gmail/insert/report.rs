//! The outcome of a `gmail insert` run.
//!
//! Mirrors `sync::report`/`extract_attachments::report`'s
//! `Report`/`Action`/`Error`/`Summary` shape (ADR-0064 Decision 4's
//! compute-render-decide pattern) — kept as its own vocabulary rather than
//! new variants on either existing enum, since neither a sync fetch nor a
//! local attachment extraction is the same operation as inserting into a
//! live mailbox.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::cli::gmail::format::{write_scalar_jsonl, JsonlSerialize};

/// Everything an insert run did (or, under `--dry-run`, would have done).
#[derive(Debug, Default, Serialize)]
pub(crate) struct InsertReport {
    pub(crate) actions: Vec<InsertAction>,
    pub(crate) errors: Vec<InsertError>,
}

impl JsonlSerialize for InsertReport {
    // One report per invocation, not a list of independent records — see
    // `SyncReport`'s identical impl.
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// One unit of work the run performed (or, under `--dry-run`, planned).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum InsertAction {
    /// A message was inserted into the destination mailbox.
    Inserted {
        id: String,
        inserted_id: String,
        label_ids: Vec<String>,
    },
    /// `--dry-run` only: a message would have been inserted with this label
    /// set.
    WouldInsert { id: String, label_ids: Vec<String> },
    /// A message was not inserted because it was already accounted for.
    Skipped { id: String, reason: SkipReason },
    /// An informational note about the run (e.g. the destination mailbox
    /// identity, or a count of messages landing in INBOX/TRASH).
    Note { message: String },
}

/// Why an [`InsertAction::Skipped`] message was left alone.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkipReason {
    /// The local ledger already has an entry for this (destination,
    /// dedupe-key) pair — a re-run of a completed or interrupted insert.
    AlreadyInserted,
    /// `--verify-remote`'s `rfc822msgid:` probe found a matching message
    /// already present on the destination.
    FoundRemote,
}

/// An error encountered while processing one message. Other messages still
/// run — see `engine::insert_all`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InsertError {
    pub(crate) id: String,
    pub(crate) reason: String,
}

/// Aggregate counts derived from `actions`/`errors` — see
/// [`InsertReport::summary`].
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct InsertSummary {
    pub(crate) inserted: usize,
    pub(crate) would_insert: usize,
    pub(crate) skipped: usize,
    pub(crate) errors: usize,
}

impl InsertReport {
    /// Tallies `actions` by variant plus `errors.len()`. Recomputed on
    /// demand, same rationale as `SyncReport::summary`.
    pub(crate) fn summary(&self) -> InsertSummary {
        let mut summary = InsertSummary {
            errors: self.errors.len(),
            ..InsertSummary::default()
        };
        for action in &self.actions {
            match action {
                InsertAction::Inserted { .. } => summary.inserted += 1,
                InsertAction::WouldInsert { .. } => summary.would_insert += 1,
                InsertAction::Skipped { .. } => summary.skipped += 1,
                // Informational only — not part of the tally.
                InsertAction::Note { .. } => {}
            }
        }
        summary
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_each_action_variant() {
        let report = InsertReport {
            actions: vec![
                InsertAction::Inserted {
                    id: "m1".to_string(),
                    inserted_id: "new1".to_string(),
                    label_ids: vec!["INBOX".to_string()],
                },
                InsertAction::WouldInsert {
                    id: "m2".to_string(),
                    label_ids: vec![],
                },
                InsertAction::Skipped {
                    id: "m3".to_string(),
                    reason: SkipReason::AlreadyInserted,
                },
                InsertAction::Note {
                    message: "note".to_string(),
                },
            ],
            errors: vec![InsertError {
                id: "m4".to_string(),
                reason: "boom".to_string(),
            }],
        };

        assert_eq!(
            report.summary(),
            InsertSummary {
                inserted: 1,
                would_insert: 1,
                skipped: 1,
                errors: 1,
            }
        );
    }

    #[test]
    fn summary_of_empty_report_is_all_zero() {
        assert_eq!(InsertReport::default().summary(), InsertSummary::default());
    }

    #[test]
    fn write_jsonl_emits_one_compact_line_for_the_whole_report() {
        let report = InsertReport {
            actions: vec![InsertAction::Note {
                message: "note".to_string(),
            }],
            errors: vec![],
        };
        let mut buf = Vec::new();
        report.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"note\""));
    }
}
