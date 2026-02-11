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
#![warn(clippy::all)]
#![deny(unsafe_code)]

pub mod claude;
pub mod cli;
pub mod data;
pub mod git;
pub mod utils;

pub use crate::cli::Cli;

/// The current version of omni-dev.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
