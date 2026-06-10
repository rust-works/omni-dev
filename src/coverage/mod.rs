//! Diff/patch coverage analysis.
//!
//! Ingests a per-line coverage report (lcov / llvm-cov JSON / cobertura) plus a
//! git diff and produces PR-attributable coverage: **patch coverage** (the
//! fraction of lines the diff added that are covered), the explicit list of
//! **uncovered new lines**, project before/after **deltas**, and **indirect**
//! coverage flips on unchanged lines.
//!
//! Pipeline:
//! 1. [`format::parse`] turns report text into a per-line [`model::CoverageReport`].
//! 2. [`diff::DiffModel::between`] builds the added-line sets and base↔head
//!    alignment from `git2`.
//! 3. [`analysis::analyze`] attributes coverage to the diff.
//! 4. [`render::render`] emits markdown / YAML / JSON.

pub mod analysis;
pub mod cobertura;
pub mod diff;
pub mod format;
pub mod lcov;
pub mod llvm_json;
pub mod model;
pub mod render;

pub use analysis::{analyze, CoverageDiff, DiffScope};
pub use diff::{default_base_ref, DiffModel};
pub use format::{parse, Format};
pub use model::{CoverageReport, FileCoverage};
pub use render::{render, OutputFormat, RenderOptions};
