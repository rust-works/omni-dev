//! Google Drive API integration: typed HTTP client, OAuth2 authentication,
//! Files/About API façades, and named-account resolution.
//!
//! Provides a thin `reqwest` wrapper around the Drive v3 REST API,
//! authenticated via OAuth2 authorization-code + PKCE (see
//! [ADR-0069](../../docs/adrs/adr-0069.md), which applies
//! [ADR-0063](../../docs/adrs/adr-0063.md) — Gmail's OAuth2
//! credential-storage design — to a second Google API). The CLI surface
//! (`src/cli/drive.rs`, issue #1524) is the first consumer; the MCP surface
//! (issue #1525) is the second, reusing the same façades.

pub mod about_api;
pub mod account;
pub mod api_client;
pub mod auth;
mod chrome_profile;
pub mod client;
pub mod content_edit;
pub mod create;
pub mod docs;
pub mod error;
pub mod file_move;
pub mod files_api;
pub mod folder_ancestry;
pub mod permissions_api;
pub mod rename;
pub mod sheets;
#[cfg(test)]
pub(crate) mod test_support;
pub mod types;
pub mod upload;
pub mod visibility;
pub mod write_gate;
