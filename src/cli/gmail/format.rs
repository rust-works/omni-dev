//! Gmail output-format helpers.
//!
//! Re-exports the shared [`crate::cli::format`] machinery (issue #1125).
//! `search` and `label list` return plain `Vec<T>`, covered by the blanket
//! `JsonlSerialize` impl. `read`/`thread` return single-record types
//! (`Message`/`Thread`), whose `JsonlSerialize` impls live beside the type
//! definitions in `src/gmail/types.rs` (see `src/datadog/types.rs` for the
//! precedent) — nothing Gmail-specific belongs in this file.

pub use crate::cli::format::*;
