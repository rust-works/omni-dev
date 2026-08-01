//! Gmail API integration: typed HTTP client and OAuth2 authentication.
//!
//! Provides a thin `reqwest` wrapper around the Gmail v1 REST API,
//! authenticated via OAuth2 authorization-code + PKCE (see
//! [ADR-0063](../../docs/adrs/adr-0063.md)). Phase 1 covers the client,
//! authentication, and the message/thread/label/history API façades; the
//! CLI and MCP surfaces land in subsequent slices.

pub mod auth;
pub mod client;
pub mod error;
pub mod history_api;
pub mod labels_api;
pub mod messages_api;
pub mod threads_api;
pub mod types;

#[cfg(test)]
pub(crate) mod test_support;
