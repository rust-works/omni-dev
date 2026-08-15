//! Google Drive API integration: typed HTTP client and OAuth2 authentication.
//!
//! Provides a thin `reqwest` wrapper around the Drive v3 REST API,
//! authenticated via OAuth2 authorization-code + PKCE (see
//! [ADR-0069](../../docs/adrs/adr-0069.md), which applies
//! [ADR-0063](../../docs/adrs/adr-0063.md) — Gmail's OAuth2
//! credential-storage design — to a second Google API). This module covers
//! the client, authentication, and named-account resolution; the CLI and
//! MCP surfaces are later child issues of #1520.
//!
//! `dead_code` is allowed crate-wide for this module tree: with no CLI/MCP
//! consumer yet (issues #1524/#1525), most of the `pub(crate)` orchestration
//! surface (account resolution, login, credential CRUD) has no caller in
//! this crate. Remove this once either child issue lands and calls into it.
#![allow(dead_code)]

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
