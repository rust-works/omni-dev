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
//! fn main() {
//!     println!("Hello from omni-dev!");
//! }
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![deny(unsafe_code)]

pub mod core;
pub mod utils;

pub use crate::core::*;

/// The current version of omni-dev
pub const VERSION: &str = env!("CARGO_PKG_VERSION");