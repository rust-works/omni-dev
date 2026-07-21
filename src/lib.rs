//! # omni-dev
//!
//! AI-powered git commit rewriter, PR generator, and MCP server for Jira,
//! Confluence, and Datadog.
//!
//! omni-dev is primarily a command-line tool; this crate also exposes the
//! library types that power it for programmatic use. See the [`cli`] module
//! for the command-line surface, and the `mcp` module (gated on the `mcp`
//! feature) for the MCP server implementation.
//!
//! ## Highlights
//!
//! - Analyse and rewrite git commit messages with a configurable AI backend
//!   (Anthropic API, AWS Bedrock, OpenAI, Ollama, or a local `claude` CLI
//!   subprocess) — see [`claude`].
//! - Generate pull-request titles and descriptions from branch history.
//! - Read, edit, and create Jira issues and Confluence pages via JFM
//!   (JIRA-Flavoured Markdown) — see [`atlassian`].
//! - Query Datadog metrics, logs, monitors, and dashboards — see [`datadog`].
//! - Expose every CLI capability as an MCP server for AI assistants by
//!   enabling the `mcp` feature.
//!
//! ## Installation
//!
//! ```text
//! cargo install omni-dev
//! omni-dev --help
//! ```
//!
//! ## Library example
//!
//! ```rust
//! use omni_dev::VERSION;
//!
//! println!("omni-dev v{VERSION}");
//! ```

#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod atlassian;
pub mod browser;
pub mod build_info;
pub mod claude;
pub mod cli;
pub mod coverage;
pub mod daemon;
pub mod data;
pub mod datadog;
pub mod git;
pub mod github_metrics;
pub mod github_rate_limit;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod pr_status;
pub mod request_log;
pub mod resources;
pub mod sessions;
pub mod snowflake;
pub mod transcript;
pub mod utils;
pub mod worktrees;

#[cfg(test)]
mod test_support;

pub use crate::cli::Cli;

/// The current version of omni-dev.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
