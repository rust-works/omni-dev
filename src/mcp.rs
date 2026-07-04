//! MCP (Model Context Protocol) server implementation.
//!
//! Exposes omni-dev's business logic to AI assistants via the MCP protocol.
//! See [ADR-0021](../../docs/adrs/adr-0021.md) for the architectural decision
//! behind the second-binary approach.

pub mod ai_tools;
pub mod atlassian_tools;
pub mod browser_tools;
pub mod cancel;
pub mod catalogue_cache;
pub mod config_tools;
pub mod confluence_tools;
pub mod content_input;
pub mod coverage_tools;
pub mod datadog_tools;
pub mod dry_run;
pub mod error;
pub mod git_tools;
pub mod jira_core_tools;
pub mod jira_tools;
pub mod log_tools;
pub mod output_file;
pub mod resources;
pub mod runtime;
pub mod server;
// Snowflake tools talk to the daemon over its Unix control socket, so — like
// `cli::snowflake` — they are Unix-only (`crate::daemon::{client,protocol,server}`
// are `#[cfg(unix)]`).
#[cfg(unix)]
pub mod snowflake_tools;
pub mod transcript_tools;
pub mod truncate;
pub mod validate;

pub use cancel::{cancellable, cancelled_error, spawn_blocking_cancellable};
pub use catalogue_cache::CatalogueCache;
pub use error::tool_error;
pub use resources::{ResourceFormat, ResourceUri, UriParseError};
pub use runtime::{
    feature_flags, log_startup_event, serve_with, try_init_tracing, write_error_chain,
};
pub use server::OmniDevServer;
pub use truncate::{truncate_response, DEFAULT_MAX_RESPONSE_BYTES};
