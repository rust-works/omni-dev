//! Contextual intelligence system for enhanced commit message analysis.

pub mod branch;
pub mod discovery;
pub mod files;
pub mod patterns;

pub use branch::BranchAnalyzer;
pub use discovery::{load_project_scopes, ProjectDiscovery};
pub use files::FileAnalyzer;
pub use patterns::WorkPatternAnalyzer;
