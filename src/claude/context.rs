//! Contextual intelligence system for enhanced commit message analysis.

pub mod branch;
pub mod discovery;
pub mod files;
pub mod patterns;

pub use branch::BranchAnalyzer;
pub use discovery::{
    config_source_label, load_config_content, load_project_scopes, resolve_context_dir,
    ConfigSourceLabel, ProjectDiscovery,
};
pub use files::FileAnalyzer;
pub use patterns::WorkPatternAnalyzer;
