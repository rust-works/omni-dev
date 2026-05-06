//! Concrete [`TranscriptSource`](crate::transcript::source::TranscriptSource)
//! implementations, one per media platform.
//!
//! No sources are wired in yet; the YouTube source lands in step 2 of the
//! [issue #687](https://github.com/rust-works/omni-dev/issues/687) build
//! order. This module exists so the public API tree is stable and so future
//! sources have a documented home that doesn't require touching format
//! converters or the trait itself.
