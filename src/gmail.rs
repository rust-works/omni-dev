//! Gmail API integration: typed HTTP client and OAuth2 authentication.
//!
//! Provides a thin `reqwest` wrapper around the Gmail v1 REST API,
//! authenticated via OAuth2 authorization-code + PKCE (see
//! [ADR-0063](../../docs/adrs/adr-0063.md)). Phase 1 covers the client,
//! authentication, and the message/thread/label API façades, plus the CLI
//! and MCP surfaces built on them. `history_api.rs` (the `historyId`
//! watermark used for incremental sync) has no Phase 1 consumer and is
//! deferred to the Phase 2 sync follow-up rather than built ahead of need.

pub mod auth;
pub mod client;
pub mod error;
pub mod labels_api;
pub mod messages_api;
pub mod threads_api;
pub mod types;

#[cfg(test)]
pub(crate) mod test_support;
