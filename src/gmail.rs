//! Gmail API integration: typed HTTP client and OAuth2 authentication.
//!
//! Provides a thin `reqwest` wrapper around the Gmail v1 REST API,
//! authenticated via OAuth2 authorization-code + PKCE (see
//! [ADR-0063](../../docs/adrs/adr-0063.md)). Phase 1 covers the client,
//! authentication, and the message/thread/label API façades, plus the CLI
//! and MCP surfaces built on them. `history_api.rs` (the `historyId`
//! watermark) and `profile_api.rs` (`users.getProfile`) back Phase 2's
//! `gmail sync` (see [ADR-0064](../../docs/adrs/adr-0064.md)).
//! `send_as_api.rs` (`users.settings.sendAs.list`) lets `gmail draft create
//! --reply-all` leave the account's own addresses out of a reply.

pub mod account;
pub mod attachments;
pub mod auth;
mod chrome_profile;
pub mod client;
pub mod compose;
pub mod draft_edit;
pub mod drafts_api;
pub mod error;
pub mod history_api;
pub mod import;
pub mod labels_api;
pub mod messages_api;
pub mod profile_api;
pub mod raw_message;
pub mod render;
pub mod send_as_api;
pub mod threads_api;
pub mod types;

#[cfg(test)]
pub(crate) mod test_support;
