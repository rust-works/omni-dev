//! Per-file diff packing into token-budget-constrained chunks.
//!
//! Groups a commit's per-file diffs into chunks that fit within a token
//! budget using first-fit-decreasing bin-packing. Files that individually
//! exceed the budget are split into per-hunk segments via
//! [`split_file_by_hunk`] and the hunks are packed the same way. A single
//! hunk that still exceeds capacity gets its own chunk; the dispatch layer
//! is responsible for treating that as a hard error.

use std::fs;

use anyhow::{Context, Result};
use tracing::debug;

use crate::claude::token_budget;
use crate::git::commit::FileDiffRef;
use crate::git::diff_split::{split_file_by_hunk, FileDiff};

/// Fraction of chunk capacity reserved as headroom for YAML serialization
/// variance and file-header overhead that the byte-count heuristic does
/// not capture.
///
/// Set to 0.70 (30% headroom) to account for YAML serialization expansion
/// which can increase content size by 20-30% for diffs containing special
/// characters, binary markers, or unusual encodings.
///
/// Mirrors [`BATCH_CAPACITY_FACTOR`](super::batch) in the commit-level
/// batching module, though uses more aggressive headroom due to observed
/// serialization variance in split dispatch scenarios.
const CHUNK_CAPACITY_FACTOR: f64 = 0.70;

/// A group of file diffs (or hunk slices) that fits within one AI
/// request's token budget.
#[derive(Debug)]
pub(crate) struct DiffChunk {
    /// Repository-relative paths of files included in this chunk.
    ///
    /// A path may appear more than once when a large file is split into
    /// hunks that land in the same chunk.
    pub file_paths: Vec<String>,
    /// Estimated total tokens for all diffs in this chunk.
    pub estimated_tokens: usize,
    /// Pre-sliced diff content for hunk items in this chunk.
    ///
    /// Parallel to `file_paths`: `diff_overrides[i]` is `Some(content)` when
    /// `file_paths[i]` was produced by hunk splitting, or `None` when the
    /// full per-file diff should be loaded from disk.
    pub diff_overrides: Vec<Option<String>>,
}

/// Result of splitting one commit's diff across N chunks.
#[derive(Debug)]
pub(crate) struct CommitDiffPlan {
    /// SHA-1 hash of the commit these chunks belong to.
    #[allow(dead_code)] // Structural metadata; useful in Debug output
    pub commit_hash: String,
    /// Ordered list of chunks, each fitting within the token budget.
    pub chunks: Vec<DiffChunk>,
}

/// A unit of diff content to be packed into a chunk.
///
/// Represents either a whole file diff or a single hunk from a split file.
struct PackableItem {
    /// Repository-relative file path.
    path: String,
    /// Estimated token cost of this item.
    estimated_tokens: usize,
    /// Pre-sliced diff content for hunk items. `None` for whole-file items.
    diff_override: Option<String>,
}

/// Packs file diffs for a single commit into chunks fitting
/// `capacity_tokens`.
///
/// Uses first-fit-decreasing by file size (same pattern as
/// [`plan_batches`](super::batch::plan_batches) in `batch.rs`). For files
/// that individually exceed `capacity_tokens`, splits into hunks via
/// [`split_file_by_hunk`] and packs hunks with the same algorithm.
///
/// Always returns at least one chunk when `file_diffs` is non-empty.
///
/// # Errors
///
/// Returns an error if a file diff cannot be read from disk when hunk
/// splitting is required.
pub(crate) fn pack_file_diffs(
    commit_hash: &str,
    file_diffs: &[FileDiffRef],
    capacity_tokens: usize,
) -> Result<CommitDiffPlan> {
    let effective_capacity = (capacity_tokens as f64 * CHUNK_CAPACITY_FACTOR) as usize;
    let items = build_packable_items(file_diffs, effective_capacity)?;
    let chunks = first_fit_decreasing(&items, effective_capacity);

    debug!(
        commit = %&commit_hash[..8.min(commit_hash.len())],
        capacity_tokens,
        effective_capacity,
        num_items = items.len(),
        num_chunks = chunks.len(),
        "pack_file_diffs: packing complete"
    );

    for (i, chunk) in chunks.iter().enumerate() {
        let oversized = chunk.estimated_tokens > effective_capacity;
        debug!(
            commit = %&commit_hash[..8.min(commit_hash.len())],
            chunk_index = i,
            estimated_tokens = chunk.estimated_tokens,
            effective_capacity,
            oversized,
            num_files = chunk.file_paths.len(),
            files = ?chunk.file_paths,
            "pack_file_diffs: chunk details"
        );
    }

    // Note: Chunks exceeding capacity are allowed - they will fail at prompt
    // validation time with a clear error. This allows the dispatch layer to
    // attempt processing and provide detailed diagnostics.

    Ok(CommitDiffPlan {
        commit_hash: commit_hash.to_string(),
        chunks,
    })
}

/// Converts file diff references into packable items, splitting oversized
/// files into per-hunk items or replacing unsplittable oversized files
/// with placeholders.
fn build_packable_items(file_diffs: &[FileDiffRef], capacity: usize) -> Result<Vec<PackableItem>> {
    let mut items = Vec::new();

    for file_ref in file_diffs {
        let file_tokens = token_budget::estimate_tokens_from_char_count(file_ref.byte_len);

        if file_tokens <= capacity {
            items.push(PackableItem {
                path: file_ref.path.clone(),
                estimated_tokens: file_tokens,
                diff_override: None,
            });
        } else {
            let hunk_items = split_oversized_file(file_ref, capacity)?;
            items.extend(hunk_items);
        }
    }

    Ok(items)
}

/// Reads a file diff from disk, splits it into hunks, and returns
/// packable items for each hunk.
///
/// Files with no hunk markers (binary files, mode-only changes) that exceed
/// capacity are replaced with a placeholder summary. Files that can be split
/// into hunks have each hunk returned as a separate packable item.
fn split_oversized_file(file_ref: &FileDiffRef, capacity: usize) -> Result<Vec<PackableItem>> {
    let content = fs::read_to_string(&file_ref.diff_file).with_context(|| {
        format!(
            "Failed to read diff file for hunk splitting: {}",
            file_ref.diff_file
        )
    })?;

    let file_diff = FileDiff {
        path: file_ref.path.clone(),
        content: content.clone(),
        byte_len: content.len(),
    };

    let hunks = split_file_by_hunk(&file_diff);

    if hunks.is_empty() {
        // No hunks found - this is likely a binary file or unsplittable content.
        // If it's still too large, replace with a placeholder.
        let file_tokens = token_budget::estimate_tokens_from_char_count(file_ref.byte_len);
        if file_tokens > capacity {
            debug!(
                path = %file_ref.path,
                file_tokens,
                capacity,
                byte_len = file_ref.byte_len,
                "Replacing oversized unsplittable file with placeholder"
            );
            let placeholder = format!(
                "diff --git a/{path} b/{path}\n\
                 [File content omitted: {bytes} bytes, estimated {tokens} tokens - exceeds capacity]\n\
                 [This file was too large to include in the analysis]\n",
                path = file_ref.path,
                bytes = file_ref.byte_len,
                tokens = file_tokens
            );
            let placeholder_tokens =
                token_budget::estimate_tokens_from_char_count(placeholder.len());
            return Ok(vec![PackableItem {
                path: file_ref.path.clone(),
                estimated_tokens: placeholder_tokens,
                diff_override: Some(placeholder),
            }]);
        }
        return Ok(vec![PackableItem {
            path: file_ref.path.clone(),
            estimated_tokens: file_tokens,
            diff_override: None,
        }]);
    }

    // Check if any individual hunk exceeds capacity - if so, replace it with placeholder
    Ok(hunks
        .iter()
        .map(|hunk| {
            let hunk_tokens = token_budget::estimate_tokens_from_char_count(hunk.byte_len);
            if hunk_tokens > capacity {
                debug!(
                    path = %file_ref.path,
                    hunk_tokens,
                    capacity,
                    byte_len = hunk.byte_len,
                    "Replacing oversized hunk with placeholder"
                );
                let placeholder = format!(
                    "{header}\
                     [Hunk content omitted: {bytes} bytes, estimated {tokens} tokens - exceeds capacity]\n",
                    header = hunk.file_header,
                    bytes = hunk.byte_len,
                    tokens = hunk_tokens
                );
                let placeholder_tokens = token_budget::estimate_tokens_from_char_count(placeholder.len());
                PackableItem {
                    path: file_ref.path.clone(),
                    estimated_tokens: placeholder_tokens,
                    diff_override: Some(placeholder),
                }
            } else {
                PackableItem {
                    path: file_ref.path.clone(),
                    estimated_tokens: hunk_tokens,
                    diff_override: Some(format!("{}{}", hunk.file_header, hunk.content)),
                }
            }
        })
        .collect())
}

/// Packs items into chunks using first-fit-decreasing bin-packing.
///
/// Items are sorted largest-first, then each is placed into the first
/// chunk with sufficient remaining capacity. Items exceeding capacity
/// get their own chunk.
#[must_use]
fn first_fit_decreasing(items: &[PackableItem], capacity: usize) -> Vec<DiffChunk> {
    let mut indexed: Vec<(usize, usize)> = items
        .iter()
        .enumerate()
        .map(|(i, item)| (i, item.estimated_tokens))
        .collect();

    // Sort descending by token estimate
    indexed.sort_by(|a, b| b.1.cmp(&a.1));

    let mut chunks: Vec<DiffChunk> = Vec::new();

    for (item_idx, item_tokens) in indexed {
        let mut placed = false;
        for chunk in &mut chunks {
            if chunk.estimated_tokens + item_tokens <= capacity {
                chunk.file_paths.push(items[item_idx].path.clone());
                chunk
                    .diff_overrides
                    .push(items[item_idx].diff_override.clone());
                chunk.estimated_tokens += item_tokens;
                placed = true;
                break;
            }
        }
        if !placed {
            chunks.push(DiffChunk {
                file_paths: vec![items[item_idx].path.clone()],
                diff_overrides: vec![items[item_idx].diff_override.clone()],
                estimated_tokens: item_tokens,
            });
        }
    }

    chunks
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::Write;

    use super::*;

    // ── test helpers ────────────────────────────────────────────

    /// Creates a single-hunk diff file and returns the `FileDiffRef` and
    /// temp file handle (kept alive for the test's lifetime).
    fn make_file_diff_ref(
        path: &str,
        content_size: usize,
    ) -> (FileDiffRef, tempfile::NamedTempFile) {
        let header = format!(
            "diff --git a/{path} b/{path}\n\
             index abc1234..def5678 100644\n\
             --- a/{path}\n\
             +++ b/{path}\n"
        );
        let hunk_header = "@@ -1,3 +1,4 @@\n";
        let body_size = content_size.saturating_sub(header.len() + hunk_header.len());
        let body = format!("{}\n", "+".repeat(body_size.max(1)));
        let content = format!("{header}{hunk_header}{body}");

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let file_ref = FileDiffRef {
            path: path.to_string(),
            diff_file: tmp.path().to_string_lossy().to_string(),
            byte_len: content.len(),
        };
        (file_ref, tmp)
    }

    /// Creates a multi-hunk diff file for testing oversized-file splitting.
    fn make_multi_hunk_file_diff_ref(
        path: &str,
        hunk_count: usize,
        bytes_per_hunk: usize,
    ) -> (FileDiffRef, tempfile::NamedTempFile) {
        let header = format!(
            "diff --git a/{path} b/{path}\n\
             index abc1234..def5678 100644\n\
             --- a/{path}\n\
             +++ b/{path}\n"
        );

        let mut content = header;
        for i in 0..hunk_count {
            let hunk_header = format!("@@ -{start},3 +{start},4 @@\n", start = i * 10 + 1);
            let body_size = bytes_per_hunk.saturating_sub(hunk_header.len());
            let body = format!("{}\n", "+".repeat(body_size.max(1)));
            content.push_str(&hunk_header);
            content.push_str(&body);
        }

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let file_ref = FileDiffRef {
            path: path.to_string(),
            diff_file: tmp.path().to_string_lossy().to_string(),
            byte_len: content.len(),
        };
        (file_ref, tmp)
    }

    // ── pack_file_diffs ────────────────────────────────────────

    #[test]
    fn pack_single_small_file() -> Result<()> {
        let (f, _tmp) = make_file_diff_ref("src/main.rs", 100);
        let plan = pack_file_diffs("abc123", &[f], 10_000)?;
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].file_paths, vec!["src/main.rs"]);
        // Whole-file items should have no diff override.
        assert_eq!(plan.chunks[0].diff_overrides, vec![None]);
        Ok(())
    }

    #[test]
    fn pack_multiple_small_files_one_chunk() -> Result<()> {
        let (f1, _t1) = make_file_diff_ref("a.rs", 100);
        let (f2, _t2) = make_file_diff_ref("b.rs", 100);
        let (f3, _t3) = make_file_diff_ref("c.rs", 100);
        let plan = pack_file_diffs("abc123", &[f1, f2, f3], 10_000)?;
        assert_eq!(plan.chunks.len(), 1, "small files should fit in one chunk");
        assert_eq!(plan.chunks[0].file_paths.len(), 3);
        Ok(())
    }

    #[test]
    fn pack_files_into_multiple_chunks() -> Result<()> {
        // Each 5000-byte file ≈ 2400 tokens (5000/2.5*1.2). Capacity 3000 tokens
        // with 0.70 headroom = effective 2100. Each file exceeds this, so gets
        // split or placeholder. Use larger capacity to ensure multiple chunks.
        let (f1, _t1) = make_file_diff_ref("a.rs", 5000);
        let (f2, _t2) = make_file_diff_ref("b.rs", 5000);
        let (f3, _t3) = make_file_diff_ref("c.rs", 5000);
        let plan = pack_file_diffs("abc123", &[f1, f2, f3], 5000)?;
        assert!(
            plan.chunks.len() >= 2,
            "expected multiple chunks, got {}",
            plan.chunks.len()
        );
        Ok(())
    }

    #[test]
    fn pack_oversized_file_splits_into_hunks() -> Result<()> {
        // 4 hunks of 500 bytes each. Each hunk + header ≈ 580 bytes ≈ 278 tokens (580/2.5*1.2).
        // Total file ≈ 2400 bytes ≈ 1152 tokens. Capacity 600, effective 420.
        // Each hunk (~278 tokens) fits within effective capacity (420).
        let (f, _tmp) = make_multi_hunk_file_diff_ref("big.rs", 4, 500);
        let plan = pack_file_diffs("abc123", &[f], 600)?;
        assert!(
            plan.chunks.len() >= 2,
            "oversized file should be split into multiple chunks, got {}",
            plan.chunks.len()
        );
        // All chunks should reference the same file
        for chunk in &plan.chunks {
            assert!(chunk.file_paths.iter().all(|p| p == "big.rs"));
            // Hunk items must carry pre-sliced diff content.
            assert_eq!(chunk.file_paths.len(), chunk.diff_overrides.len());
            for ovr in &chunk.diff_overrides {
                assert!(ovr.is_some(), "hunk items should have diff_override set");
                let content = ovr.as_ref().unwrap();
                assert!(
                    content.contains("diff --git"),
                    "override should contain file header"
                );
                assert!(
                    content.contains("@@"),
                    "override should contain hunk marker"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn pack_single_hunk_exceeding_capacity_gets_placeholder() -> Result<()> {
        // One file with a single massive hunk — cannot be split further.
        // Now replaced with a small placeholder instead of being oversized.
        let (f, _tmp) = make_file_diff_ref("huge.rs", 10_000);
        let plan = pack_file_diffs("abc123", &[f], 100)?;
        assert_eq!(plan.chunks.len(), 1);
        // The chunk now contains a placeholder, so tokens are small
        assert!(
            plan.chunks[0].estimated_tokens < 100,
            "placeholder should have small token count, got {}",
            plan.chunks[0].estimated_tokens
        );
        // Verify it has a diff_override (the placeholder)
        assert!(
            plan.chunks[0].diff_overrides[0].is_some(),
            "oversized file should have placeholder override"
        );
        let override_content = plan.chunks[0].diff_overrides[0].as_ref().unwrap();
        // File has a hunk, so it goes through hunk splitting and gets "Hunk content omitted"
        assert!(
            override_content.contains("content omitted"),
            "placeholder should indicate content was omitted, got: {override_content}"
        );
        Ok(())
    }

    #[test]
    fn pack_mixed_small_and_oversized() -> Result<()> {
        let (small1, _t1) = make_file_diff_ref("small1.rs", 100);
        let (small2, _t2) = make_file_diff_ref("small2.rs", 100);
        // 3 hunks of 1000 bytes; total ≈ 3080 bytes ≈ 968 tokens.
        // Capacity 500, effective 450 → file must be split into hunks.
        // Each hunk + header ≈ 1080 bytes ≈ 339 tokens — fits in 450.
        let (big, _t3) = make_multi_hunk_file_diff_ref("big.rs", 3, 1000);
        let plan = pack_file_diffs("abc123", &[small1, small2, big], 500)?;
        // Small files should pack together; big file's hunks distribute
        let total_paths: usize = plan.chunks.iter().map(|c| c.file_paths.len()).sum();
        assert!(
            total_paths >= 4,
            "expected at least 4 items (2 small + 3 hunks from big), got {total_paths}"
        );
        // Verify overrides: whole-file items = None, hunk items = Some.
        for chunk in &plan.chunks {
            assert_eq!(chunk.file_paths.len(), chunk.diff_overrides.len());
            for (path, ovr) in chunk.file_paths.iter().zip(chunk.diff_overrides.iter()) {
                if path == "big.rs" {
                    assert!(ovr.is_some(), "hunk items for big.rs should have override");
                } else {
                    assert!(ovr.is_none(), "whole-file items should have no override");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn pack_empty_input() -> Result<()> {
        let plan = pack_file_diffs("abc123", &[], 10_000)?;
        assert!(plan.chunks.is_empty());
        Ok(())
    }

    #[test]
    fn pack_preserves_commit_hash() -> Result<()> {
        let (f, _tmp) = make_file_diff_ref("a.rs", 100);
        let plan = pack_file_diffs("deadbeef42", &[f], 10_000)?;
        assert_eq!(plan.commit_hash, "deadbeef42");
        Ok(())
    }

    #[test]
    fn pack_file_read_error() {
        // Nonexistent diff_file with byte_len large enough to trigger hunk splitting.
        let file_ref = FileDiffRef {
            path: "missing.rs".to_string(),
            diff_file: "/nonexistent/path/to/diff.txt".to_string(),
            byte_len: 100_000,
        };
        let result = pack_file_diffs("abc123", &[file_ref], 100);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to read diff file"),
            "expected descriptive error, got: {err_msg}"
        );
    }

    #[test]
    fn pack_binary_file_no_hunks() -> Result<()> {
        // Binary file diff has no @@ markers — split_file_by_hunk returns empty.
        let content = "diff --git a/image.png b/image.png\n\
                        new file mode 100644\n\
                        index 0000000..abc1234\n\
                        Binary files /dev/null and b/image.png differ\n";

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let file_ref = FileDiffRef {
            path: "image.png".to_string(),
            diff_file: tmp.path().to_string_lossy().to_string(),
            byte_len: content.len(),
        };

        // Set capacity low enough that the file triggers hunk splitting
        let plan = pack_file_diffs("abc123", &[file_ref], 10)?;
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].file_paths, vec!["image.png"]);
        Ok(())
    }

    // ── first_fit_decreasing ───────────────────────────────────

    #[test]
    fn ffd_largest_placed_first() {
        let items = vec![
            PackableItem {
                path: "small.rs".to_string(),
                estimated_tokens: 10,
                diff_override: None,
            },
            PackableItem {
                path: "large.rs".to_string(),
                estimated_tokens: 90,
                diff_override: Some("large-content".to_string()),
            },
            PackableItem {
                path: "medium.rs".to_string(),
                estimated_tokens: 50,
                diff_override: None,
            },
        ];
        let chunks = first_fit_decreasing(&items, 100);
        // large (90) goes first, then medium (50) won't fit with large,
        // but small (10) will → 2 chunks: [large, small] and [medium]
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].file_paths.contains(&"large.rs".to_string()));
        assert!(chunks[0].file_paths.contains(&"small.rs".to_string()));
        assert_eq!(chunks[1].file_paths, vec!["medium.rs"]);
        // Verify diff_overrides are parallel and propagated correctly.
        for chunk in &chunks {
            assert_eq!(chunk.file_paths.len(), chunk.diff_overrides.len());
        }
        // large.rs has override, small.rs does not
        let large_idx = chunks[0]
            .file_paths
            .iter()
            .position(|p| p == "large.rs")
            .unwrap();
        assert!(chunks[0].diff_overrides[large_idx].is_some());
        let small_idx = chunks[0]
            .file_paths
            .iter()
            .position(|p| p == "small.rs")
            .unwrap();
        assert!(chunks[0].diff_overrides[small_idx].is_none());
    }

    #[test]
    fn ffd_empty_items() {
        let chunks = first_fit_decreasing(&[], 100);
        assert!(chunks.is_empty());
    }

    // ── diff_override preservation ────────────────────────────────

    #[test]
    fn split_oversized_preserves_each_hunk_content() -> Result<()> {
        // 3 hunks, each ~500 bytes ≈ 240 tokens (500/2.5*1.2).
        // Capacity 500, effective 350. Each hunk fits but file total doesn't.
        let (f, _tmp) = make_multi_hunk_file_diff_ref("big.rs", 3, 500);
        let plan = pack_file_diffs("abc123", &[f], 500)?;

        // Collect all overrides across chunks.
        let all_overrides: Vec<&String> = plan
            .chunks
            .iter()
            .flat_map(|c| c.diff_overrides.iter())
            .filter_map(|o| o.as_ref())
            .collect();

        assert_eq!(all_overrides.len(), 3, "should have 3 hunk overrides");
        // Each override should be self-contained with file header + hunk.
        for (i, ovr) in all_overrides.iter().enumerate() {
            assert!(
                ovr.starts_with("diff --git"),
                "hunk {i} override should start with file header"
            );
            assert!(
                ovr.contains("@@"),
                "hunk {i} override should contain hunk marker"
            );
        }
        // Overrides should be distinct (different hunks).
        let unique: std::collections::HashSet<&str> =
            all_overrides.iter().map(|s| s.as_str()).collect();
        assert_eq!(unique.len(), 3, "each hunk override should be distinct");
        Ok(())
    }

    #[test]
    fn binary_file_within_capacity_has_no_override() -> Result<()> {
        let content = "diff --git a/image.png b/image.png\n\
                        new file mode 100644\n\
                        index 0000000..abc1234\n\
                        Binary files /dev/null and b/image.png differ\n";

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let file_ref = FileDiffRef {
            path: "image.png".to_string(),
            diff_file: tmp.path().to_string_lossy().to_string(),
            byte_len: content.len(),
        };

        // Capacity high enough that binary file fits without placeholder.
        // 125 bytes ≈ 60 tokens (125/2.5*1.2). Need capacity > 60/0.70 ≈ 86.
        let plan = pack_file_diffs("abc123", &[file_ref], 200)?;
        assert_eq!(plan.chunks.len(), 1);
        // Binary file within capacity has no hunks → whole-file item → None override.
        assert_eq!(plan.chunks[0].diff_overrides, vec![None]);
        Ok(())
    }

    #[test]
    fn binary_file_exceeding_capacity_gets_placeholder() -> Result<()> {
        let content = "diff --git a/image.png b/image.png\n\
                        new file mode 100644\n\
                        index 0000000..abc1234\n\
                        Binary files /dev/null and b/image.png differ\n";

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let file_ref = FileDiffRef {
            path: "image.png".to_string(),
            diff_file: tmp.path().to_string_lossy().to_string(),
            byte_len: content.len(),
        };

        // Capacity low enough to trigger placeholder.
        let plan = pack_file_diffs("abc123", &[file_ref], 10)?;
        assert_eq!(plan.chunks.len(), 1);
        // Binary file exceeding capacity gets placeholder override.
        assert!(plan.chunks[0].diff_overrides[0].is_some());
        let override_content = plan.chunks[0].diff_overrides[0].as_ref().unwrap();
        assert!(override_content.contains("File content omitted"));
        Ok(())
    }
}
