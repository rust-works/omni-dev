//! A minimal async token bucket for pacing request *rate*, distinct from
//! bounding request *concurrency* (`tokio::sync::Semaphore`).
//!
//! Generic and domain-agnostic by design — no knowledge of Gmail or any
//! other quota lives here; callers size it for their own budget (see
//! `src/gmail/messages_api.rs`'s `GMAIL_QUOTA_UNITS_PER_SECOND`). A
//! hand-rolled bucket was chosen over a crate like `governor`: the actual
//! need (one shared counter, lazily refilled from elapsed time, behind a
//! mutex) is small enough that pulling in a dependency with its own clock
//! abstraction for a single call site isn't worth the added surface.

use tokio::sync::Mutex;
use tokio::time::Instant;

struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

/// Paces callers against a `capacity`-unit budget that refills continuously
/// at `refill_per_sec` units/second.
///
/// Unlike [`tokio::sync::Semaphore`], [`Self::acquire`] returns nothing —
/// once spent, a token is never "released" back regardless of whether the
/// caller's subsequent work succeeds, fails, or is retried.
///
/// Uses [`tokio::time::Instant`], not [`std::time::Instant`] — this is what
/// lets tests drive it deterministically via `tokio::time::pause`/`advance`
/// instead of real sleeps.
pub(crate) struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<BucketState>,
}

impl TokenBucket {
    /// Builds a bucket that starts full — a fresh run gets its full first
    /// burst immediately, matching how a real per-second quota window
    /// behaves at t=0.
    pub(crate) fn new(capacity_units: u32, refill_per_sec: u32) -> Self {
        let capacity = f64::from(capacity_units);
        Self {
            capacity,
            refill_per_sec: f64::from(refill_per_sec),
            state: Mutex::new(BucketState {
                tokens: capacity,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Waits until `cost` units are available, then debits them.
    ///
    /// `cost` is clamped to the bucket's capacity — requesting more than
    /// capacity in one call would otherwise wait forever, since tokens can
    /// never exceed it.
    pub(crate) async fn acquire(&self, cost: u32) {
        let cost = f64::from(cost).min(self.capacity);
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                state.tokens = elapsed
                    .mul_add(self.refill_per_sec, state.tokens)
                    .min(self.capacity);
                state.last_refill = now;

                if state.tokens >= cost {
                    state.tokens -= cost;
                    None
                } else {
                    let deficit = cost - state.tokens;
                    Some(std::time::Duration::from_secs_f64(
                        deficit / self.refill_per_sec,
                    ))
                }
            };
            match wait {
                None => return,
                Some(duration) => tokio::time::sleep(duration).await,
            }
        }
    }

    /// Test-only introspection: the current token count (a real snapshot
    /// behind the same mutex `acquire` locks, just without forcing a refill
    /// computation first). Lets a caller's tests assert *how much* was
    /// debited without needing real elapsed time to observe pacing.
    #[cfg(test)]
    pub(crate) async fn available(&self) -> f64 {
        self.state.lock().await.tokens
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn burst_within_capacity_resolves_instantly() {
        let bucket = TokenBucket::new(250, 250);
        let start = Instant::now();
        for _ in 0..50 {
            bucket.acquire(5).await;
        }
        assert_eq!(Instant::now() - start, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn acquisition_past_capacity_waits_the_exact_deficit() {
        let bucket = TokenBucket::new(250, 250);
        // Drain the bucket completely.
        bucket.acquire(250).await;
        let start = Instant::now();
        // Deficit is `5/250s = 20ms` for the next 5-unit acquisition.
        bucket.acquire(5).await;
        assert_eq!(Instant::now() - start, Duration::from_millis(20));
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_rate_never_exceeds_the_configured_budget() {
        let bucket = TokenBucket::new(250, 250);
        let start = Instant::now();
        let n = 100u32;
        for _ in 0..n {
            bucket.acquire(5).await;
        }
        let elapsed = Instant::now() - start;
        // n * 5 units total, budget is 250 units/sec => elapsed must be at
        // least (n*5 - capacity)/250 seconds (the first `capacity` worth is
        // free from the initial full bucket).
        let min_elapsed_secs = f64::from(n).mul_add(5.0, -250.0).max(0.0) / 250.0;
        assert!(elapsed.as_secs_f64() >= min_elapsed_secs - f64::EPSILON);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_acquirers_share_the_same_ceiling() {
        let bucket = Arc::new(TokenBucket::new(250, 250));
        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..20 {
            let bucket = Arc::clone(&bucket);
            handles.push(tokio::spawn(async move {
                for _ in 0..5 {
                    bucket.acquire(5).await;
                }
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        // 20 tasks * 5 acquisitions * 5 units = 500 units total; the bucket
        // starts with 250 free, so at least 250 units' worth (1s) must have
        // been paced regardless of how the tasks interleaved.
        let elapsed = Instant::now() - start;
        assert!(elapsed >= Duration::from_secs(1) - Duration::from_millis(1));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_above_capacity_is_clamped_and_completes() {
        let bucket = TokenBucket::new(250, 250);
        tokio::time::timeout(Duration::from_secs(5), bucket.acquire(10_000))
            .await
            .expect("acquire(cost > capacity) must not hang forever");
    }
}
