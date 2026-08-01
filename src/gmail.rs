//! Gmail API integration: typed HTTP client and OAuth2 authentication.
//!
//! Provides a thin `reqwest` wrapper around the Gmail v1 REST API,
//! authenticated via OAuth2 authorization-code + PKCE (see
//! [ADR-0063](../../docs/adrs/adr-0063.md)). Phase 1 covers the client and
//! authentication only; the message/thread/label/history API façades land in
//! a subsequent slice.

pub mod auth;
pub mod client;
pub mod error;

#[cfg(test)]
pub(crate) mod test_support;
