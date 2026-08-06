//! The outcome of a `gmail extract-attachments` run.
//!
//! Mirrors `src/cli/gmail/sync/report.rs`'s `SyncReport`/`SyncAction`/
//! `SyncError` shape (ADR-0064 Decision 4's compute-render-decide pattern):
//! per-record outcomes accumulate here rather than aborting the run, so one
//! unreadable `.eml` never discards an otherwise-successful batch. Kept as
//! its own small type rather than new `SyncAction` variants — this isn't a
//! sync run, and the two vocabularies shouldn't blur.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::cli::gmail::format::{write_scalar_jsonl, JsonlSerialize};

/// Everything an extract-attachments run did (or, under `--dry-run`, would
/// have done).
#[derive(Debug, Default, Serialize)]
pub(crate) struct ExtractAttachmentsReport {
    pub(crate) actions: Vec<ExtractAction>,
    pub(crate) errors: Vec<ExtractError>,
}

impl JsonlSerialize for ExtractAttachmentsReport {
    // One report per invocation, not a list of independent records — see
    // `SyncReport`'s identical impl.
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// One unit of work the run performed for a candidate message (one whose
/// manifest record had `attachment_count > 0` and no `attachments/` dir
/// yet).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ExtractAction {
    /// Attachments were extracted and written to disk.
    Extracted { id: String, count: usize },
    /// `--dry-run` only: attachments would have been extracted.
    WouldExtract { id: String, count: usize },
}

/// An error encountered while processing one message's `.eml`. Other
/// messages still run — see `engine::run_extract_attachments`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExtractError {
    pub(crate) id: String,
    pub(crate) reason: String,
}

/// Aggregate counts derived from `actions`/`errors` — see
/// [`ExtractAttachmentsReport::summary`].
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ExtractSummary {
    pub(crate) extracted: usize,
    pub(crate) would_extract: usize,
    pub(crate) errors: usize,
}

impl ExtractAttachmentsReport {
    /// Tallies `actions` by variant plus `errors.len()`. Recomputed on
    /// demand, same rationale as `SyncReport::summary`.
    pub(crate) fn summary(&self) -> ExtractSummary {
        let mut summary = ExtractSummary {
            errors: self.errors.len(),
            ..ExtractSummary::default()
        };
        for action in &self.actions {
            match action {
                ExtractAction::Extracted { .. } => summary.extracted += 1,
                ExtractAction::WouldExtract { .. } => summary.would_extract += 1,
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
        let report = ExtractAttachmentsReport {
            actions: vec![
                ExtractAction::Extracted {
                    id: "m1".to_string(),
                    count: 2,
                },
                ExtractAction::WouldExtract {
                    id: "m2".to_string(),
                    count: 1,
                },
            ],
            errors: vec![ExtractError {
                id: "m3".to_string(),
                reason: "boom".to_string(),
            }],
        };

        assert_eq!(
            report.summary(),
            ExtractSummary {
                extracted: 1,
                would_extract: 1,
                errors: 1,
            }
        );
    }

    #[test]
    fn summary_of_empty_report_is_all_zero() {
        assert_eq!(
            ExtractAttachmentsReport::default().summary(),
            ExtractSummary::default()
        );
    }
}
