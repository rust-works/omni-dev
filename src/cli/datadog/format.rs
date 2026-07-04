//! Datadog output-format helpers.
//!
//! Re-exports the shared [`crate::cli::format`] machinery (issue #1125). Datadog
//! commands serialize plain `Vec<T>` payloads, which the shared module already
//! covers via its blanket `JsonlSerialize` impl, so no Datadog-specific impls are
//! needed here.

pub use crate::cli::format::*;
