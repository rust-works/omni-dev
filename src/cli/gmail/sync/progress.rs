//! Structured progress events for `gmail sync`'s full-mailbox pass (#1502).
//!
//! [`SyncProgressEvent`] is the boundary: `engine.rs` may emit these over a
//! caller-supplied channel but never renders anything itself, so it keeps
//! performing no direct stdout/stderr I/O (ADR-0064's amendment for #1502).
//! Only the CLI layer (`src/cli/gmail/sync.rs`) may turn these into
//! something a user sees.

/// One update `run_sync_with_progress` may emit while a full-mailbox
/// listing+fetch pass is running.
#[derive(Debug, Clone)]
pub(crate) enum SyncProgressEvent {
    /// A `messages.list` page arrived. Both fields are running totals (a
    /// fact about "where listing is now"), not deltas.
    ListingPage { pages: usize, ids_discovered: usize },
    /// Listing has finished; no further `ListingPage` events will follow.
    ListingDone,
    /// One fetch was dispatched — grows the fetch bar's known total by one.
    FetchQueued,
    /// One fetch finished (success or failure) — advances the fetch bar's
    /// position by one.
    FetchCompleted { failed: bool },
}
