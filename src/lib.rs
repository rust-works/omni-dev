//! # omni-dev
//!
//! A comprehensive development toolkit written in Rust.
//!
//! ## Features
//!
//! - Fast and efficient development tools
//! - Extensible architecture
//! - Memory safe and reliable
//!
//! ## Quick Start
//!
//! ```rust
//! use omni_dev::*;
//!
//! println!("Hello from omni-dev!");
//! ```

#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod atlassian;
pub mod browser;
pub mod claude;
pub mod cli;
pub mod coverage;
pub mod data;
pub mod datadog;
pub mod git;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod resources;
pub mod transcript;
pub mod utils;
pub mod voice;

#[cfg(test)]
mod test_support;

pub use crate::cli::Cli;

/// The current version of omni-dev.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
