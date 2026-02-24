//! Token-budget-aware commit batching for the map phase.
//!
//! Groups commits into batches that fit within the AI model's token budget
//! using a first-fit-decreasing bin-packing algorithm. Each batch becomes
//! a single AI request, reducing round-trips while progressive diff
//! reduction provides a safety net if estimates are slightly off.

use crate::claude::ai::AiClientMetadata;
use crate::claude::token_budget::{self, TokenBudget};
use crate::git::commit::CommitInfo;

/// Fraction of available token budget used as batch capacity.
///
/// Leaves 10% headroom for YAML serialization variance (indentation,
/// literal block markers) that shifts actual token counts beyond the
/// character-based estimate.
const BATCH_CAPACITY_FACTOR: f64 = 0.90;

/// Estimated token overhead per commit for non-diff metadata fields.
///
/// Accounts for hash (40 chars), author (~50 chars), date (~30 chars),
/// analysis fields (~200 chars), and YAML formatting overhead.
const PER_COMMIT_METADATA_OVERHEAD_TOKENS: usize = 120;

/// Estimated token overhead for the `RepositoryViewForAI` YAML envelope.
///
/// Accounts for versions, explanation, working_directory, remotes, ai,
/// and branch_info fields in the serialized YAML.
const VIEW_ENVELOPE_OVERHEAD_TOKENS: usize = 150;

/// A group of commits to send in one AI request.
#[derive(Debug)]
pub(crate) struct CommitBatch {
    /// Indices into the original commit list (preserves ordering context).
    pub commit_indices: Vec<usize>,
    /// Estimated total tokens for all commits in this batch.
    pub estimated_tokens: usize,
}

/// Result of planning how to batch commits.
#[derive(Debug)]
pub(crate) struct BatchPlan {
    /// Ordered list of batches.
    pub batches: Vec<CommitBatch>,
}

/// Estimates the token cost of a single commit for batching purposes.
///
/// Uses `fs::metadata` (stat syscall) for the diff file size to avoid
/// reading all diff files into memory during planning. Falls back to
/// zero if the file is inaccessible.
#[must_use]
fn estimate_commit_tokens(commit: &CommitInfo) -> usize {
    let diff_file_size = std::fs::metadata(&commit.analysis.diff_file)
        .map(|m| m.len() as usize)
        .unwrap_or(0);

    let text_len = diff_file_size
        + commit.analysis.diff_summary.len()
        + commit.original_message.len()
        + commit.analysis.proposed_message.len();

    token_budget::estimate_tokens_from_char_count(text_len) + PER_COMMIT_METADATA_OVERHEAD_TOKENS
}

/// Groups commits into batches that fit within the model's token budget.
///
/// Uses first-fit-decreasing bin-packing: sorts commits largest-first,
/// then places each into the first batch with remaining capacity.
/// Oversized commits get solo batches and rely on progressive diff
/// reduction at request time.
#[must_use]
pub(crate) fn plan_batches(
    commits: &[CommitInfo],
    metadata: &AiClientMetadata,
    system_prompt_tokens: usize,
) -> BatchPlan {
    let budget = TokenBudget::from_metadata(metadata);
    let raw_capacity = budget
        .available_input_tokens()
        .saturating_sub(system_prompt_tokens)
        .saturating_sub(VIEW_ENVELOPE_OVERHEAD_TOKENS);
    let capacity = (raw_capacity as f64 * BATCH_CAPACITY_FACTOR) as usize;

    // Estimate per-commit tokens and pair with original index
    let mut indexed_estimates: Vec<(usize, usize)> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| (i, estimate_commit_tokens(c)))
        .collect();

    // Sort descending by token estimate (first-fit-decreasing)
    indexed_estimates.sort_by(|a, b| b.1.cmp(&a.1));

    let mut batches: Vec<CommitBatch> = Vec::new();

    for (commit_idx, commit_tokens) in indexed_estimates {
        let mut placed = false;
        for batch in &mut batches {
            if batch.estimated_tokens + commit_tokens <= capacity {
                batch.commit_indices.push(commit_idx);
                batch.estimated_tokens += commit_tokens;
                placed = true;
                break;
            }
        }
        if !placed {
            batches.push(CommitBatch {
                commit_indices: vec![commit_idx],
                estimated_tokens: commit_tokens,
            });
        }
    }

    BatchPlan { batches }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::Write;

    use chrono::Utc;

    use super::*;
    use crate::claude::ai::AiClientMetadata;
    use crate::git::commit::{CommitAnalysis, FileChange, FileChanges};

    fn make_metadata(context: usize, response: usize) -> AiClientMetadata {
        AiClientMetadata {
            provider: "test".to_string(),
            model: "test-model".to_string(),
            max_context_length: context,
            max_response_length: response,
            active_beta: None,
        }
    }

    fn make_commit_with_diff_file(diff_size: usize) -> (CommitInfo, tempfile::NamedTempFile) {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = "x".repeat(diff_size);
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let commit = CommitInfo {
            hash: "abc123".to_string(),
            author: "Test Author <test@example.com>".to_string(),
            date: Utc::now().fixed_offset(),
            original_message: "test commit".to_string(),
            in_main_branches: vec![],
            analysis: CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: "test".to_string(),
                proposed_message: "feat(test): test".to_string(),
                file_changes: FileChanges {
                    total_files: 1,
                    files_added: 0,
                    files_deleted: 0,
                    file_list: vec![FileChange {
                        status: "M".to_string(),
                        file: "test.rs".to_string(),
                    }],
                },
                diff_summary: "test.rs | 10 ++++".to_string(),
                diff_file: tmp.path().to_string_lossy().to_string(),
                file_diffs: Vec::new(),
            },
        };
        (commit, tmp)
    }

    #[test]
    fn plan_batches_single_commit() {
        let (commit, _tmp) = make_commit_with_diff_file(100);
        let metadata = make_metadata(200_000, 64_000);
        let plan = plan_batches(&[commit], &metadata, 1000);
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batches[0].commit_indices, vec![0]);
    }

    #[test]
    fn plan_batches_all_fit_one_batch() {
        let (c1, _t1) = make_commit_with_diff_file(100);
        let (c2, _t2) = make_commit_with_diff_file(200);
        let (c3, _t3) = make_commit_with_diff_file(150);
        let metadata = make_metadata(200_000, 64_000);
        let plan = plan_batches(&[c1, c2, c3], &metadata, 1000);
        assert_eq!(
            plan.batches.len(),
            1,
            "small commits should fit in one batch"
        );
        assert_eq!(plan.batches[0].commit_indices.len(), 3);
    }

    #[test]
    fn plan_batches_large_commits_get_solo() {
        // Each commit takes ~150k tokens worth of diff — way over budget per batch
        // available = 200k - 64k - 1000 - 150 = 134,850, * 0.9 = ~121,365
        // Each 500k char commit ≈ 157k tokens — exceeds capacity individually
        let (c1, _t1) = make_commit_with_diff_file(500_000);
        let (c2, _t2) = make_commit_with_diff_file(500_000);
        let metadata = make_metadata(200_000, 64_000);
        let plan = plan_batches(&[c1, c2], &metadata, 1000);
        assert_eq!(
            plan.batches.len(),
            2,
            "large commits should get separate batches"
        );
        assert_eq!(plan.batches[0].commit_indices.len(), 1);
        assert_eq!(plan.batches[1].commit_indices.len(), 1);
    }

    #[test]
    fn plan_batches_mixed_sizes() {
        // Budget: available = 200k - 64k = 136k, - overhead ~1150 = ~134,850, * 0.9 = ~121,365
        // Large commit: 300k chars ≈ 94k tokens + 120 overhead ≈ 94k
        // Small commits: 100 chars ≈ 32 tokens + 120 overhead ≈ 152 each
        // Large fills most of a batch, smalls should pack together or with large
        let (large, _t1) = make_commit_with_diff_file(300_000);
        let (s1, _t2) = make_commit_with_diff_file(100);
        let (s2, _t3) = make_commit_with_diff_file(100);
        let (s3, _t4) = make_commit_with_diff_file(100);
        let metadata = make_metadata(200_000, 64_000);
        let plan = plan_batches(&[large, s1, s2, s3], &metadata, 1000);
        // Large commit goes first (sorted), smalls should fit with it
        assert!(
            plan.batches.len() <= 2,
            "expected at most 2 batches, got {}",
            plan.batches.len()
        );
    }

    #[test]
    fn estimate_commit_tokens_scales_with_diff_size() {
        let (small, _t1) = make_commit_with_diff_file(100);
        let (large, _t2) = make_commit_with_diff_file(10_000);
        let small_tokens = estimate_commit_tokens(&small);
        let large_tokens = estimate_commit_tokens(&large);
        assert!(
            large_tokens > small_tokens,
            "large diff ({large_tokens}) should have more tokens than small ({small_tokens})"
        );
    }
}
