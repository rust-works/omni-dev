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
pub mod auth;
mod chrome_profile;
pub mod client;
pub mod error;
pub mod files_api;
#[cfg(test)]
pub(crate) mod test_support;
pub mod types;
