//! Utility functions and helpers

pub mod ai_scratch;
pub mod general;
pub mod settings;

// Re-export commonly used items from general
pub use general::*;
pub use settings::{get_env_var, get_env_vars, Settings};
