//! Google Docs API (v1) integration — reading the *structural model* of a
//! Google Doc, which the Drive API structurally cannot expose (issue #1615,
//! [ADR-0071](../../docs/adrs/adr-0071.md) §9).
//!
//! Drive treats a Doc as an opaque native document: `files.export` renders it
//! to a foreign format and a media `files.update` replaces the whole document
//! by re-importing one. Neither has any notion of a paragraph, a table cell
//! or an index — which is why [ADR-0071](../../docs/adrs/adr-0071.md) §9 has
//! `drive edit` refuse Google-native documents outright, and why
//! `drive read --content` can only hand back a one-way markdown rendering
//! with no path back to the document model.
//!
//! That model is the whole point of this module. Every Docs mutation is
//! addressed by `{tabId, segmentId, index}`, so an export — however faithful
//! its prose — cannot tell you *where* to edit. `drive read --content` stays
//! the prose channel; `drive docs read` is the model channel.
//!
//! Lives under `src/drive/` rather than a top-level `src/docs/` for the same
//! two reasons [`crate::drive::sheets`] does, one of them load-bearing: it
//! shares the account store, `DriveCredentials`, token refresh and the
//! `drive` commit scope; and `FilesApi`'s mutating methods are
//! `pub(in crate::drive)` — the visibility fence that makes "no gate bypass"
//! true by construction rather than by convention. A sibling top-level module
//! could not call them without widening that fence.

pub mod api;
pub mod client;
pub mod structure;
pub mod types;
