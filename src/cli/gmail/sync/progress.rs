//! Structured progress events for `gmail sync`'s full-mailbox pass, and the
//! `indicatif` rendering that turns them into live stderr bars (#1502).
//!
//! [`SyncProgressEvent`] is the boundary: `engine.rs` may emit these over a
//! caller-supplied channel but never imports `indicatif` itself, so it keeps
//! performing no direct stdout/stderr I/O (ADR-0064's amendment for #1502).
//! Only [`SyncProgressBars`], constructed and driven exclusively by
//! `src/cli/gmail/sync.rs`, actually renders anything.

use std::time::Duration;

use tokio::sync::mpsc;

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

/// The two live `indicatif` bars for a `gmail sync` run (listing spinner,
/// fetch bar) and the task that drains [`SyncProgressEvent`]s into them.
pub(crate) struct SyncProgressBars {
    listing: indicatif::ProgressBar,
    fetch: indicatif::ProgressBar,
    // Keeps both bars registered/coordinated for the run's lifetime; never
    // read again after construction, but must outlive both bars.
    _multi: indicatif::MultiProgress,
}

impl SyncProgressBars {
    pub(crate) fn new() -> Self {
        let multi = indicatif::MultiProgress::new();

        let listing = multi.add(indicatif::ProgressBar::new_spinner());
        listing.set_style(listing_style());
        listing.enable_steady_tick(Duration::from_millis(100));
        listing.set_message("0 pages, 0 ids found");

        let fetch = multi.add(indicatif::ProgressBar::new(0));
        fetch.set_style(fetch_style());

        Self {
            listing,
            fetch,
            _multi: multi,
        }
    }

    /// Drains `rx` until the sender side is dropped (the sync run finished
    /// or failed), updating both bars as events arrive, then clears the
    /// listing spinner and finishes the fetch bar in place.
    pub(crate) async fn drain(self, mut rx: mpsc::UnboundedReceiver<SyncProgressEvent>) {
        let mut errors = 0usize;
        while let Some(event) = rx.recv().await {
            match event {
                SyncProgressEvent::ListingPage {
                    pages,
                    ids_discovered,
                } => {
                    self.listing
                        .set_message(format!("{pages} pages, {ids_discovered} ids found"));
                }
                SyncProgressEvent::ListingDone => self.listing.finish_and_clear(),
                SyncProgressEvent::FetchQueued => self.fetch.inc_length(1),
                SyncProgressEvent::FetchCompleted { failed } => {
                    if failed {
                        errors += 1;
                        self.fetch.set_message(format!("({errors} errors)"));
                    }
                    self.fetch.inc(1);
                }
            }
        }
        // Safety net if the channel closed before a `ListingDone` was sent
        // (e.g. an error aborted the run mid-listing) — both calls are
        // idempotent, so this is a no-op when `ListingDone` already fired.
        self.listing.finish_and_clear();
        self.fetch.finish();
    }
}

#[allow(clippy::expect_used)] // Compile-time constant template literal
fn listing_style() -> indicatif::ProgressStyle {
    indicatif::ProgressStyle::with_template("{spinner:.cyan} Listing mailbox… {msg}")
        .expect("valid indicatif template literal")
}

#[allow(clippy::expect_used)] // Compile-time constant template literal
fn fetch_style() -> indicatif::ProgressStyle {
    indicatif::ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} messages fetched {msg}")
        .expect("valid indicatif template literal")
        .progress_chars("##-")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `ProgressBar` is a cheap `Clone` handle onto shared state (see
    /// `indicatif::ProgressBar::clone`), so cloning `bars.listing`/`.fetch`
    /// out *before* handing `bars` itself into [`SyncProgressBars::drain`]
    /// (which consumes `self`) is what lets these tests inspect the bars'
    /// final position/length/message after the drain loop exits.
    #[tokio::test]
    async fn listing_page_events_update_the_spinner_message() {
        let bars = SyncProgressBars::new();
        let listing = bars.listing.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(SyncProgressEvent::ListingPage {
            pages: 3,
            ids_discovered: 150,
        })
        .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(listing.message(), "3 pages, 150 ids found");
    }

    #[tokio::test]
    async fn listing_done_finishes_and_clears_the_spinner() {
        let bars = SyncProgressBars::new();
        let listing = bars.listing.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(SyncProgressEvent::ListingDone).unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert!(listing.is_finished());
    }

    #[tokio::test]
    async fn fetch_queued_grows_length_and_fetch_completed_advances_position() {
        let bars = SyncProgressBars::new();
        let fetch = bars.fetch.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        // Realistic ordering: `fetch_and_archive_messages_streaming` never
        // returns (closing this channel) with a dispatched fetch still
        // in flight, so every `FetchQueued` here has a matching
        // `FetchCompleted` — otherwise `drain`'s unconditional `finish()`
        // safety net would snap `position` to `length` regardless, which
        // is exercised separately below.
        tx.send(SyncProgressEvent::FetchQueued).unwrap();
        tx.send(SyncProgressEvent::FetchQueued).unwrap();
        tx.send(SyncProgressEvent::FetchCompleted { failed: false })
            .unwrap();
        tx.send(SyncProgressEvent::FetchCompleted { failed: false })
            .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(fetch.length(), Some(2));
        assert_eq!(fetch.position(), 2);
        assert_eq!(fetch.message(), "");
    }

    #[tokio::test]
    async fn failed_fetches_set_a_running_error_count_message() {
        let bars = SyncProgressBars::new();
        let fetch = bars.fetch.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        for _ in 0..2 {
            tx.send(SyncProgressEvent::FetchQueued).unwrap();
            tx.send(SyncProgressEvent::FetchCompleted { failed: true })
                .unwrap();
        }
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(fetch.position(), 2);
        assert_eq!(fetch.message(), "(2 errors)");
    }

    #[tokio::test]
    async fn both_bars_finish_even_when_the_channel_closes_before_listing_done() {
        // Mirrors a run aborting mid-listing (e.g. a `messages.list` error):
        // the sender drops with no `ListingDone` ever sent — the safety net
        // after the drain loop must still leave both bars finished.
        let bars = SyncProgressBars::new();
        let listing = bars.listing.clone();
        let fetch = bars.fetch.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(SyncProgressEvent::ListingPage {
            pages: 1,
            ids_discovered: 5,
        })
        .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert!(listing.is_finished());
        assert!(fetch.is_finished());
    }
}
