//! The outcome of a `gmail sync` run.
//!
//! Modelled directly on `src/cli/ai/claude/history/sync.rs`'s
//! `SyncReport`/`SyncAction`/`SyncError`: per-item outcomes accumulate here
//! rather than aborting the run, so one unfetchable message never discards
//! an hour of otherwise-successful work.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::cli::gmail::format::{write_scalar_jsonl, JsonlSerialize};

/// Everything a sync run did (or, under `--dry-run`, would have done).
#[derive(Debug, Default, Serialize)]
pub(crate) struct SyncReport {
    pub(crate) actions: Vec<SyncAction>,
    pub(crate) errors: Vec<SyncError>,
}

impl JsonlSerialize for SyncReport {
    // One report per invocation, not a list of independent records — a
    // scalar line, like `Message`/`Thread`'s impls, not the `Vec<T>`
    // blanket impl `search`/`label list` get for free.
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// One unit of work the sync performed.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SyncAction {
    /// A message was fetched and archived.
    Fetched {
        id: String,
        path: PathBuf,
        bytes: u64,
    },
    /// `--dry-run` only: a message would have been fetched.
    WouldFetch { id: String },
    /// A `labelsAdded`/`labelsRemoved` history event updated the manifest.
    LabelsUpdated {
        id: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// A message no longer appears on the server; the manifest record is
    /// soft-deleted (the `.eml` is never removed).
    Deleted { id: String },
    /// A previously soft-deleted message reappeared in a listing.
    Undeleted { id: String },
    /// `--dry-run` only: a message would have been soft-deleted.
    WouldDelete { id: String },
    /// `--dry-run` only: a message would have been undeleted.
    WouldUndelete { id: String },
    /// A message listed by `history.list` no longer exists on the server by
    /// the time it was fetched (`messages.get` 404, reason `notFound`) — the
    /// history event was stale. Not treated as an error: unlike [`Deleted`],
    /// no manifest record exists yet to soft-delete (#1509).
    ///
    /// [`Deleted`]: SyncAction::Deleted
    Vanished { id: String },
    /// An informational note about the run (e.g. why reconciliation ran).
    Note { message: String },
}

/// An error encountered while processing one message. Other messages still
/// run — see `src/cli/gmail/sync/engine.rs`'s fetch loop.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncError {
    pub(crate) id: String,
    pub(crate) reason: String,
}

/// Aggregate counts derived from `actions`/`errors` — see [`SyncReport::summary`].
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct SyncSummary {
    pub(crate) fetched: usize,
    pub(crate) would_fetch: usize,
    pub(crate) vanished: usize,
    pub(crate) labels_updated: usize,
    pub(crate) deleted: usize,
    pub(crate) undeleted: usize,
    pub(crate) would_delete: usize,
    pub(crate) would_undelete: usize,
    pub(crate) errors: usize,
}

impl SyncReport {
    /// Tallies `actions` by variant plus `errors.len()`. Recomputed on
    /// demand — `actions`/`errors` are pushed to directly throughout
    /// `engine::run_sync`, so there is no single point to accumulate a
    /// stored counter without risking drift.
    pub(crate) fn summary(&self) -> SyncSummary {
        let mut summary = SyncSummary {
            errors: self.errors.len(),
            ..SyncSummary::default()
        };
        for action in &self.actions {
            match action {
                SyncAction::Fetched { .. } => summary.fetched += 1,
                SyncAction::WouldFetch { .. } => summary.would_fetch += 1,
                SyncAction::LabelsUpdated { .. } => summary.labels_updated += 1,
                SyncAction::Deleted { .. } => summary.deleted += 1,
                SyncAction::Undeleted { .. } => summary.undeleted += 1,
                SyncAction::WouldDelete { .. } => summary.would_delete += 1,
                SyncAction::WouldUndelete { .. } => summary.would_undelete += 1,
                SyncAction::Vanished { .. } => summary.vanished += 1,
                // Informational only — not part of the tally (#1488).
                SyncAction::Note { .. } => {}
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
        let report = SyncReport {
            actions: vec![
                SyncAction::Fetched {
                    id: "m1".to_string(),
                    path: PathBuf::from("m1.eml"),
                    bytes: 1,
                },
                SyncAction::WouldFetch {
                    id: "m2".to_string(),
                },
                SyncAction::LabelsUpdated {
                    id: "m3".to_string(),
                    added: vec!["IMPORTANT".to_string()],
                    removed: vec![],
                },
                SyncAction::Deleted {
                    id: "m4".to_string(),
                },
                SyncAction::Undeleted {
                    id: "m5".to_string(),
                },
                SyncAction::WouldDelete {
                    id: "m6".to_string(),
                },
                SyncAction::WouldUndelete {
                    id: "m7".to_string(),
                },
                SyncAction::Vanished {
                    id: "m8".to_string(),
                },
                SyncAction::Note {
                    message: "one note".to_string(),
                },
                SyncAction::Note {
                    message: "another note".to_string(),
                },
            ],
            errors: vec![SyncError {
                id: "m9".to_string(),
                reason: "boom".to_string(),
            }],
        };

        assert_eq!(
            report.summary(),
            SyncSummary {
                fetched: 1,
                would_fetch: 1,
                vanished: 1,
                labels_updated: 1,
                deleted: 1,
                undeleted: 1,
                would_delete: 1,
                would_undelete: 1,
                errors: 1,
            }
        );
    }

    #[test]
    fn summary_of_empty_report_is_all_zero() {
        assert_eq!(SyncReport::default().summary(), SyncSummary::default());
    }
}
