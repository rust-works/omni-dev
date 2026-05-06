//! Concrete [`TranscriptSource`](crate::transcript::source::TranscriptSource)
//! implementations, one per media platform.
//!
//! Each submodule houses one source. Format converters
//! ([`crate::transcript::format`]) and the trait itself
//! ([`crate::transcript::source`]) are source-agnostic and are not touched
//! when a new source is added — see [`youtube`] for the layout to follow.

pub mod youtube;
