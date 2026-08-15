//! Drive output-format helpers.
//!
//! Re-exports the shared [`crate::cli::format`] machinery (issue #1125).
//! `search` returns a plain `Vec<DriveFile>`, covered by the blanket
//! `JsonlSerialize` impl. `read`'s metadata mode returns a single-record
//! type (`DriveFile`), whose `JsonlSerialize` impl lives beside the type
//! definition in `src/drive/types.rs` (mirrors `src/gmail/types.rs`) —
//! nothing Drive-specific belongs in this file.

pub use crate::cli::format::*;
