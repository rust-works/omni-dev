//! Shared baseline for both spike prototypes:
//!   - `wer()` — Levenshtein edit-distance word error rate
//!   - `IdleDetector` — silence-gap RMS endpoint (copied verbatim from
//!     `src/voice/idle.rs` to keep spike build self-contained, no
//!     transitive deps on the parent `omni-dev` crate)
//!   - `Event` — the JSONL event-log schema both prototypes emit

pub mod events;
pub mod idle;
pub mod wer;
