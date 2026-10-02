//! Google Slides object graph and guarded text replacement (ADR-0093).
//! Lives under Drive to share accounts, OAuth transport and the mutation
//! visibility fence; Slides object IDs have no Docs index-model counterpart.

pub mod api;
pub mod client;
pub mod read;
pub mod target;
pub mod types;
pub mod write;
pub mod write_types;
