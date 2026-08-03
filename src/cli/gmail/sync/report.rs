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
