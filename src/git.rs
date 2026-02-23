//! Git operations and repository management.

pub mod amendment;
pub mod commit;
pub mod diff_split;
pub mod remote;
pub mod repository;

pub use amendment::AmendmentHandler;
pub use commit::{CommitAnalysis, CommitAnalysisForAI, CommitInfo, CommitInfoForAI};
pub use diff_split::{split_by_file, split_file_by_hunk, FileDiff, HunkDiff};
pub use remote::RemoteInfo;
pub use repository::GitRepository;

/// Number of hex characters to show in abbreviated commit hashes.
pub const SHORT_HASH_LEN: usize = 8;

/// Length of a full SHA-1 commit hash in hex characters.
pub const FULL_HASH_LEN: usize = 40;
