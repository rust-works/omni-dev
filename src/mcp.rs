//! MCP (Model Context Protocol) server implementation.
//!
//! Exposes omni-dev's business logic to AI assistants via the MCP protocol.
//! See [ADR-0021](../../docs/adrs/adr-0021.md) for the architectural decision
//! behind the second-binary approach.

pub mod ai_tools;
pub mod atlassian_tools;
pub mod config_tools;
pub mod confluence_tools;
pub mod error;
pub mod git_tools;
pub mod jira_core_tools;
pub mod jira_tools;
pub mod resources;
pub mod runtime;
pub mod server;

pub use error::tool_error;
pub use resources::{ResourceFormat, ResourceUri, UriParseError};
pub use runtime::{serve_with, try_init_tracing, write_error_chain};
pub use server::OmniDevServer;
