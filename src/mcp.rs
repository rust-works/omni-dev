//! MCP (Model Context Protocol) server implementation.
//!
//! Exposes omni-dev's business logic to AI assistants via the MCP protocol.
//! See [ADR-0021](../../docs/adrs/adr-0021.md) for the architectural decision
//! behind the second-binary approach.

pub mod error;
pub mod git_tools;
pub mod server;

pub use error::tool_error;
pub use server::OmniDevServer;
