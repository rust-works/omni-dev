//! Structured progress events for `gmail sync`'s full-mailbox pass, and the
//! `indicatif` rendering that turns them into live stderr bars (#1502).
//!
//! [`SyncProgressEvent`] is the boundary: `engine.rs` may emit these over a
//! caller-supplied channel but never imports `indicatif` itself, so it keeps
//! performing no direct stdout/stderr I/O (ADR-0064's amendment for #1502).
//! Only [`SyncProgressBars`], constructed and driven by `src/cli/gmail/
//! sync.rs` (one bar pair, its own `MultiProgress`) and `src/cli/gmail/
//! sync_all.rs` (one bar pair per account, all registered on one shared
//! `MultiProgress` via [`SyncProgressBars::new_in`] — ADR-0068's Decision 4
//! follow-up, #1504), actually renders anything.

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
    /// A rate-limit-retryable response (Gmail's 429 or 403 quota-exhaustion
    /// signal) is about to wait before retrying (#1651). Rendered as a
    /// transient message on the account's own fetch bar — already prefixed
    /// with the account label in `gmail sync-all` — instead of the shared
    /// retry driver's default raw `eprintln!`, which would tear a live
    /// `MultiProgress` render and carry no account attribution.
    RateLimited {
        status: u16,
        delay_secs: u64,
        attempt: u32,
    },
}

/// The two live `indicatif` bars for a `gmail sync` run (listing spinner,
/// fetch bar) and the task that drains [`SyncProgressEvent`]s into them.
pub(crate) struct SyncProgressBars {
    listing: indicatif::ProgressBar,
    fetch: indicatif::ProgressBar,
    // Keeps both bars registered/coordinated for the run's lifetime (it must
    // outlive both bars), and is what [`SyncProgressBars::drain_and_remove`]
    // detaches them from.
    multi: indicatif::MultiProgress,
}

impl SyncProgressBars {
    pub(crate) fn new() -> Self {
        Self::build(indicatif::MultiProgress::new(), None)
    }

    /// One listing-spinner + fetch-bar pair registered on an existing
    /// `MultiProgress`, each row prefixed with `label`.
    ///
    /// Lets `gmail sync-all` (ADR-0068) render every concurrently-syncing
    /// account's two bars under one shared terminal area instead of each
    /// account's [`SyncProgressBars::new`] fighting over stderr with its
    /// own `MultiProgress` — `MultiProgress` is a cheap `Clone` (an `Arc`
    /// handle), so cloning it into each account's `SyncProgressBars` just
    /// keeps that shared instance alive, it doesn't create a second one.
    pub(crate) fn new_in(multi: &indicatif::MultiProgress, label: &str) -> Self {
        Self::build(multi.clone(), Some(label))
    }

    fn build(multi: indicatif::MultiProgress, label: Option<&str>) -> Self {
        let prefixed = label.is_some();

        let listing = multi.add(indicatif::ProgressBar::new_spinner());
        listing.set_style(listing_style(prefixed));
        if let Some(label) = label {
            listing.set_prefix(label.to_string());
        }
        listing.enable_steady_tick(Duration::from_millis(100));
        listing.set_message("0 pages, 0 ids found");

        let fetch = multi.add(indicatif::ProgressBar::new(0));
        fetch.set_style(fetch_style(prefixed));
        if let Some(label) = label {
            fetch.set_prefix(label.to_string());
        }

        Self {
            listing,
            fetch,
            multi,
        }
    }

    /// Drains `rx` until the sender side is dropped (the sync run finished
    /// or failed), updating both bars as events arrive, then clears the
    /// listing spinner and finishes the fetch bar in place.
    pub(crate) async fn drain(self, rx: mpsc::UnboundedReceiver<SyncProgressEvent>) {
        self.apply_events(rx).await;
        // Safety net if the channel closed before a `ListingDone` was sent
        // (e.g. an error aborted the run mid-listing) — both calls are
        // idempotent, so this is a no-op when `ListingDone` already fired.
        self.listing.finish_and_clear();
        self.fetch.finish();
    }

    /// [`SyncProgressBars::drain`]'s `gmail sync-all` counterpart (#1652):
    /// once `rx` closes, clears *both* bars and detaches them from the
    /// shared `MultiProgress`, instead of leaving the fetch bar finished in
    /// place.
    ///
    /// A bar that is dropped while still registered becomes an indicatif
    /// "zombie", whose on-screen lines the next `suspend` wipes. indicatif
    /// 0.18 counts a head-of-list zombie's lines once per *rate-limited*
    /// draw, not once, so while other accounts' bars keep ticking the count
    /// grows past the lines the bars occupy — and the next `suspend` then
    /// erases stdout lines above them: an earlier account's summary. An
    /// explicitly removed bar never becomes a zombie. The account's summary
    /// line, printed right after this returns, supersedes the fetch bar's
    /// final count.
    ///
    /// The detach runs from a drop guard that owns the bars from the moment
    /// this is called — not from the future's first poll — so it also
    /// happens if the render task panics, or is cancelled before or during
    /// the drain: the cases where a zombie would otherwise be left behind.
    pub(crate) fn drain_and_remove(
        self,
        rx: mpsc::UnboundedReceiver<SyncProgressEvent>,
    ) -> impl std::future::Future<Output = ()> {
        let bars = DetachOnDrop(self);
        async move { bars.0.apply_events(rx).await }
    }

    /// The event loop shared by [`SyncProgressBars::drain`] and
    /// [`SyncProgressBars::drain_and_remove`]: updates both bars until the
    /// sender side of `rx` is dropped.
    async fn apply_events(&self, mut rx: mpsc::UnboundedReceiver<SyncProgressEvent>) {
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
                    }
                    // Always reset the message from the running error count
                    // on completion — clears a transient `RateLimited`
                    // notice left over from a retry that has since resolved.
                    self.fetch.set_message(if errors > 0 {
                        format!("({errors} errors)")
                    } else {
                        String::new()
                    });
                    self.fetch.inc(1);
                }
                SyncProgressEvent::RateLimited {
                    status,
                    delay_secs,
                    attempt,
                } => {
                    self.fetch.set_message(format!(
                        "rate limited ({status}), retrying in {delay_secs}s (attempt {attempt})"
                    ));
                }
            }
        }
    }
}

/// [`SyncProgressBars::drain_and_remove`]'s drop guard: clears both bars and
/// detaches them from their `MultiProgress`, on every exit path.
struct DetachOnDrop(SyncProgressBars);

impl Drop for DetachOnDrop {
    fn drop(&mut self) {
        for bar in [&self.0.listing, &self.0.fetch] {
            bar.finish_and_clear();
            self.0.multi.remove(bar);
        }
    }
}

/// `prefixed` selects a `{prefix}`-leading template for [`SyncProgressBars::
/// new_in`]'s multi-account rows; [`SyncProgressBars::new`]'s single-account
/// bars keep the plain template byte-identical to before #1504.
// The `{spinner}`/`{msg}`/`{prefix}` placeholders are indicatif's own
// template syntax, not a Rust format string.
#[allow(clippy::expect_used, clippy::literal_string_with_formatting_args)] // Compile-time constant template literals
fn listing_style(prefixed: bool) -> indicatif::ProgressStyle {
    let template = if prefixed {
        "{prefix:.bold} {spinner:.cyan} Listing mailbox… {msg}"
    } else {
        "{spinner:.cyan} Listing mailbox… {msg}"
    };
    indicatif::ProgressStyle::with_template(template).expect("valid indicatif template literal")
}

#[allow(clippy::expect_used, clippy::literal_string_with_formatting_args)] // Compile-time constant template literals
fn fetch_style(prefixed: bool) -> indicatif::ProgressStyle {
    let template = if prefixed {
        "{prefix:.bold} {bar:40.cyan/blue} {pos}/{len} messages fetched {msg}"
    } else {
        "{bar:40.cyan/blue} {pos}/{len} messages fetched {msg}"
    };
    indicatif::ProgressStyle::with_template(template)
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

    // ── RateLimited (#1651) ───────────────────────────────────────────

    #[tokio::test]
    async fn rate_limited_event_sets_the_fetch_bar_message() {
        let bars = SyncProgressBars::new();
        let fetch = bars.fetch.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(SyncProgressEvent::RateLimited {
            status: 429,
            delay_secs: 2,
            attempt: 1,
        })
        .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(
            fetch.message(),
            "rate limited (429), retrying in 2s (attempt 1)"
        );
    }

    #[tokio::test]
    async fn fetch_completed_after_rate_limited_clears_the_message() {
        let bars = SyncProgressBars::new();
        let fetch = bars.fetch.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(SyncProgressEvent::FetchQueued).unwrap();
        tx.send(SyncProgressEvent::RateLimited {
            status: 403,
            delay_secs: 4,
            attempt: 1,
        })
        .unwrap();
        tx.send(SyncProgressEvent::FetchCompleted { failed: false })
            .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(fetch.message(), "");
    }

    // ── new_in (#1504, ADR-0068's Decision 4 follow-up) ──────────────────

    #[test]
    fn new_in_prefixes_both_bars_with_the_account_label() {
        let multi = indicatif::MultiProgress::new();
        let bars = SyncProgressBars::new_in(&multi, "jky.greens");

        assert_eq!(bars.listing.prefix(), "jky.greens");
        assert_eq!(bars.fetch.prefix(), "jky.greens");
    }

    #[tokio::test]
    async fn new_in_two_accounts_drain_independently_on_one_shared_multi_progress() {
        let multi = indicatif::MultiProgress::new();
        let account_a = SyncProgressBars::new_in(&multi, "acct-a");
        let account_b = SyncProgressBars::new_in(&multi, "acct-b");
        let fetch_a = account_a.fetch.clone();
        let fetch_b = account_b.fetch.clone();

        let (tx_a, rx_a) = mpsc::unbounded_channel();
        let (tx_b, rx_b) = mpsc::unbounded_channel();
        tx_a.send(SyncProgressEvent::FetchQueued).unwrap();
        tx_a.send(SyncProgressEvent::FetchCompleted { failed: false })
            .unwrap();
        drop(tx_a);
        tx_b.send(SyncProgressEvent::FetchQueued).unwrap();
        tx_b.send(SyncProgressEvent::FetchQueued).unwrap();
        tx_b.send(SyncProgressEvent::FetchCompleted { failed: true })
            .unwrap();
        tx_b.send(SyncProgressEvent::FetchCompleted { failed: false })
            .unwrap();
        drop(tx_b);

        // Both accounts' bars live on the *same* `MultiProgress` (the whole
        // point of #1504's shared-instance design, vs. each account's
        // `SyncProgressBars::new` fighting over stderr with its own), yet
        // their `drain` loops and bar state stay per-account-independent.
        account_a.drain(rx_a).await;
        account_b.drain(rx_b).await;

        assert_eq!(fetch_a.position(), 1);
        assert_eq!(fetch_b.position(), 2);
        assert_eq!(fetch_b.length(), Some(2));
        assert_eq!(fetch_b.message(), "(1 errors)");
    }

    // ── drain_and_remove (#1652) ─────────────────────────────────────────

    #[tokio::test]
    async fn drain_and_remove_finishes_both_bars() {
        let multi = visible_multi_progress();
        let bars = SyncProgressBars::new_in(&multi, "acct");
        let listing = bars.listing.clone();
        let fetch = bars.fetch.clone();
        assert!(!listing.is_hidden() && !fetch.is_hidden());
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(SyncProgressEvent::FetchQueued).unwrap();
        tx.send(SyncProgressEvent::FetchCompleted { failed: true })
            .unwrap();
        drop(tx);
        bars.drain_and_remove(rx).await;

        assert!(listing.is_finished());
        assert!(fetch.is_finished());
        // `MultiProgress::remove` swaps in a hidden draw target — the
        // detachment that keeps a dropped bar from becoming a zombie.
        assert!(listing.is_hidden());
        assert!(fetch.is_hidden());
        assert_eq!(fetch.position(), 1);
        assert_eq!(fetch.message(), "(1 errors)");
    }

    #[tokio::test]
    async fn drain_and_remove_detaches_both_bars_when_cancelled_mid_drain() {
        let multi = visible_multi_progress();
        let bars = SyncProgressBars::new_in(&multi, "acct");
        let listing = bars.listing.clone();
        let fetch = bars.fetch.clone();
        assert!(!listing.is_hidden() && !fetch.is_hidden());
        let (tx, rx) = mpsc::unbounded_channel::<SyncProgressEvent>();

        // `tx` stays open, so the drain can only end by being aborted — and
        // on this current-thread runtime the abort lands before the task is
        // ever polled, the case a guard built inside an `async fn` body
        // would miss.
        let render_task = tokio::spawn(bars.drain_and_remove(rx));
        render_task.abort();
        assert!(render_task.await.unwrap_err().is_cancelled());

        assert!(listing.is_hidden());
        assert!(fetch.is_hidden());
        drop(tx);
    }

    /// A `MultiProgress` drawing to an in-memory terminal. Unlike
    /// `MultiProgress::new()`, whose stderr target is hidden whenever the
    /// test's stderr isn't a tty, its bars start out *not* hidden — so a
    /// test can observe `MultiProgress::remove` hiding them.
    fn visible_multi_progress() -> indicatif::MultiProgress {
        indicatif::MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::term_like(
            Box::new(indicatif::InMemoryTerm::new(10, 100)),
        ))
    }

    /// The non-blank lines on `term`'s screen, top to bottom.
    fn screen_lines(term: &indicatif::InMemoryTerm) -> Vec<String> {
        term.contents()
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Replays one account's whole run (`fetched` successful fetches) and
    /// closes its channel, the way `run_one_account` returning does.
    async fn finish_account(bars: SyncProgressBars, fetched: usize) {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(SyncProgressEvent::ListingDone).unwrap();
        for _ in 0..fetched {
            tx.send(SyncProgressEvent::FetchQueued).unwrap();
            tx.send(SyncProgressEvent::FetchCompleted { failed: false })
                .unwrap();
        }
        drop(tx);
        bars.drain_and_remove(rx).await;
    }

    #[tokio::test]
    async fn drain_and_remove_keeps_every_summary_line_while_other_bars_draw() {
        // The #1652 sequence. With `drain` (finished bars dropped while still
        // registered) this erases `b: summary` and `b`'s bar: `b`'s zombie
        // reaches the head of the list when `a` finishes, `c`'s throttled
        // `inc` draws each re-count its lines, and the next `suspend` clears
        // that inflated count — reaching above the bars into the stdout
        // lines. A 1 Hz draw target makes nearly every non-forced draw a
        // throttled one.
        let term = indicatif::InMemoryTerm::new(50, 100);
        let multi = indicatif::MultiProgress::with_draw_target(
            indicatif::ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 1),
        );
        let a = SyncProgressBars::new_in(&multi, "a");
        let b = SyncProgressBars::new_in(&multi, "b");
        let c = SyncProgressBars::new_in(&multi, "c");
        let c_fetch = c.fetch.clone();
        c_fetch.inc_length(100);

        finish_account(b, 2).await;
        multi.suspend(|| indicatif::TermLike::write_line(&term, "b: summary").unwrap());
        finish_account(a, 1).await;
        for _ in 0..50 {
            c_fetch.inc(1);
        }
        multi.suspend(|| indicatif::TermLike::write_line(&term, "a: summary").unwrap());
        finish_account(c, 0).await;
        multi.suspend(|| indicatif::TermLike::write_line(&term, "c: summary").unwrap());
        indicatif::TermLike::write_line(&term, "combined").unwrap();

        assert_eq!(
            screen_lines(&term),
            ["b: summary", "a: summary", "c: summary", "combined"]
        );
    }
}
