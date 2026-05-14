//! Pluggable RNG for [`EventId`](super::EventId) (ULID) generation.
//!
//! Lives at `voice/` scope rather than under `backends/mock.rs` because
//! the same trait is consumed by [`voice reflect`](super::reflect) for
//! deterministic event minting in snapshot tests. Production code uses
//! [`SystemUlidRng`]; tests use [`CountingUlidRng`].

use ulid::Ulid;

/// Source of [`Ulid`]s. Pluggable so tests can pin ULIDs while production
/// uses real entropy.
pub trait UlidRng: Send + Sync {
    /// Returns the next ULID. Each call must produce a value strictly
    /// greater than the previous one to preserve event-id monotonicity.
    fn next_ulid(&mut self) -> Ulid;
}

/// Production RNG: defers to `Ulid::new()`.
#[derive(Debug, Default)]
pub struct SystemUlidRng;

impl UlidRng for SystemUlidRng {
    fn next_ulid(&mut self) -> Ulid {
        Ulid::new()
    }
}

/// Deterministic RNG for tests.
///
/// Returns `Ulid::from_parts(0, counter)` for an increasing counter
/// starting at 1. The encoded form is lexicographically ordered, so a
/// sequence of ULIDs from this RNG is monotonically increasing — exactly
/// the property snapshot tests rely on.
#[derive(Debug, Default)]
pub struct CountingUlidRng {
    counter: u128,
}

impl CountingUlidRng {
    /// Builds a new counting RNG starting at zero — the first ULID it
    /// returns has random bits `1`, the second `2`, and so on.
    pub fn new() -> Self {
        Self { counter: 0 }
    }
}

impl UlidRng for CountingUlidRng {
    fn next_ulid(&mut self) -> Ulid {
        self.counter += 1;
        Ulid::from_parts(0, self.counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_ulid_rng_produces_unique_values() {
        let mut rng = SystemUlidRng;
        let a = rng.next_ulid();
        let b = rng.next_ulid();
        assert_ne!(a, b);
    }

    #[test]
    fn counting_ulid_rng_starts_at_one() {
        let mut rng = CountingUlidRng::new();
        let first = rng.next_ulid();
        assert_eq!(first, Ulid::from_parts(0, 1));
    }

    #[test]
    fn counting_ulid_rng_is_monotonic() {
        let mut rng = CountingUlidRng::new();
        let a = rng.next_ulid();
        let b = rng.next_ulid();
        let c = rng.next_ulid();
        assert!(a < b);
        assert!(b < c);
    }
}
