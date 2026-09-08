//! Structured progress events for `gmail insert`, and the `indicatif`
//! rendering that turns them into a live stderr bar.
//!
//! A new event/bar type rather than reusing `sync::progress::SyncProgressEvent`:
//! `gmail sync`'s full pass is two-phase with a total unknowable until
//! listing ends (hence its listing spinner + separately-growing fetch bar),
//! while `gmail insert`'s selection is resolved entirely locally before the
//! first network call — the total is exact from the start, so one
//! determinate bar (no spinner phase) is all this needs. `engine.rs` may
//! emit these over a caller-supplied channel but never imports `indicatif`
//! itself, mirroring `sync`'s UI-agnostic-engine split (ADR-0064's
//! amendment for #1502).

use tokio::sync::mpsc;

/// One update `run_insert_with_progress` may emit while a run is in
/// progress.
#[derive(Debug, Clone)]
pub(crate) enum InsertProgressEvent {
    /// The plan is resolved — sets the bar's total length. Sent once, before
    /// any per-message event.
    Started { total: usize },
    /// One message finished processing (inserted, skipped, or failed) —
    /// advances the bar's position by one.
    Completed { failed: bool },
    /// A rate-limit-retryable response is about to wait before retrying
    /// (mirrors `sync::progress::SyncProgressEvent::RateLimited`) — routed
    /// here instead of the shared retry driver's default `eprintln!`, which
    /// would tear a live bar render.
    RateLimited {
        status: u16,
        delay_secs: u64,
        attempt: u32,
    },
}

/// The single live `indicatif` bar for a `gmail insert` run, and the task
/// that drains [`InsertProgressEvent`]s into it.
pub(crate) struct InsertProgressBar {
    bar: indicatif::ProgressBar,
}

impl InsertProgressBar {
    pub(crate) fn new() -> Self {
        let bar = indicatif::ProgressBar::new(0);
        bar.set_style(bar_style());
        Self { bar }
    }

    /// Drains `rx` until the sender side is dropped (the run finished or
    /// failed), updating the bar as events arrive, then finishes it in
    /// place.
    pub(crate) async fn drain(self, mut rx: mpsc::UnboundedReceiver<InsertProgressEvent>) {
        let mut errors = 0usize;
        while let Some(event) = rx.recv().await {
            match event {
                InsertProgressEvent::Started { total } => {
                    self.bar.set_length(total as u64);
                }
                InsertProgressEvent::Completed { failed } => {
                    if failed {
                        errors += 1;
                    }
                    // Always reset the message from the running error count
                    // on completion — clears a transient `RateLimited`
                    // notice left over from a retry that has since resolved.
                    self.bar.set_message(if errors > 0 {
                        format!("({errors} errors)")
                    } else {
                        String::new()
                    });
                    self.bar.inc(1);
                }
                InsertProgressEvent::RateLimited {
                    status,
                    delay_secs,
                    attempt,
                } => {
                    self.bar.set_message(format!(
                        "rate limited ({status}), retrying in {delay_secs}s (attempt {attempt})"
                    ));
                }
            }
        }
        self.bar.finish();
    }
}

// The `{bar}`/`{pos}`/`{len}`/`{msg}` placeholders are indicatif's own
// template syntax, not a Rust format string.
#[allow(clippy::expect_used, clippy::literal_string_with_formatting_args)] // Compile-time constant template literal
fn bar_style() -> indicatif::ProgressStyle {
    indicatif::ProgressStyle::with_template(
        "{bar:40.cyan/blue} {pos}/{len} messages inserted {msg}",
    )
    .expect("valid indicatif template literal")
    .progress_chars("##-")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn started_sets_the_bar_length() {
        let bars = InsertProgressBar::new();
        let bar = bars.bar.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(InsertProgressEvent::Started { total: 5 }).unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(bar.length(), Some(5));
    }

    #[tokio::test]
    async fn completed_advances_position() {
        let bars = InsertProgressBar::new();
        let bar = bars.bar.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(InsertProgressEvent::Started { total: 2 }).unwrap();
        tx.send(InsertProgressEvent::Completed { failed: false })
            .unwrap();
        tx.send(InsertProgressEvent::Completed { failed: false })
            .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(bar.position(), 2);
        assert_eq!(bar.message(), "");
    }

    #[tokio::test]
    async fn failed_completions_set_a_running_error_count_message() {
        let bars = InsertProgressBar::new();
        let bar = bars.bar.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(InsertProgressEvent::Started { total: 2 }).unwrap();
        tx.send(InsertProgressEvent::Completed { failed: true })
            .unwrap();
        tx.send(InsertProgressEvent::Completed { failed: true })
            .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(bar.message(), "(2 errors)");
    }

    #[tokio::test]
    async fn rate_limited_sets_the_bar_message() {
        let bars = InsertProgressBar::new();
        let bar = bars.bar.clone();
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(InsertProgressEvent::RateLimited {
            status: 429,
            delay_secs: 2,
            attempt: 1,
        })
        .unwrap();
        drop(tx);
        bars.drain(rx).await;

        assert_eq!(
            bar.message(),
            "rate limited (429), retrying in 2s (attempt 1)"
        );
    }

    #[tokio::test]
    async fn drain_finishes_the_bar_when_the_channel_closes() {
        let bars = InsertProgressBar::new();
        let bar = bars.bar.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);
        bars.drain(rx).await;
        assert!(bar.is_finished());
    }
}
