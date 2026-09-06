//! Google Sheets API (v4) integration — reading and writing spreadsheet
//! *cells*, which the Drive API structurally cannot do (issue #1589,
//! [ADR-0073](../../docs/adrs/adr-0073.md)).
//!
//! Drive treats a Sheet as an opaque native document: `files.export` renders
//! it to a foreign format and a media `files.update` replaces the whole
//! document by re-importing one. There is no Drive-level notion of a range,
//! a row or a cell, which is why [ADR-0069](../../docs/adrs/adr-0069.md) can
//! only export the *first* sheet as CSV and why
//! [ADR-0071](../../docs/adrs/adr-0071.md) §9 has `drive edit` refuse
//! Google-native documents outright.
//!
//! Lives under `src/drive/` rather than a top-level `src/sheets/` for two
//! reasons, one of them load-bearing: it shares the account store,
//! `DriveCredentials`, token refresh and the `drive` commit scope; and
//! `FilesApi`'s mutating methods are `pub(in crate::drive)` — the visibility
//! fence that makes "no gate bypass" true by construction rather than by
//! convention. A sibling top-level module could not call them without
//! widening that fence.

pub mod a1;
pub mod api;
pub mod client;
pub mod create;
pub mod read;
pub mod structure;
pub mod types;
pub mod write;
