//! Per-file and per-hunk unified diff splitting.

/// Marker that begins a per-file section in unified diff output.
const FILE_DIFF_MARKER: &str = "diff --git a/";

/// Marker that begins a hunk within a file diff.
const HUNK_MARKER: &str = "@@ ";

/// A per-file slice of a unified diff.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Path of the file (extracted from the `b/` side of `diff --git a/... b/...`).
    pub path: String,
    /// Raw text of this file's diff (header + all hunks).
    pub content: String,
    /// Byte length of `content`.
    pub byte_len: usize,
}

/// A single hunk within one file's diff.
#[derive(Debug, Clone)]
pub struct HunkDiff {
    /// The file header lines (`diff --git`, `index`, `---`, `+++`).
    pub file_header: String,
    /// Raw text of this hunk starting from the `@@` line.
    pub content: String,
    /// Byte length of `file_header` + `content` combined.
    pub byte_len: usize,
}

/// Splits a flat unified diff at `diff --git a/` boundaries.
///
/// Returns one [`FileDiff`] for each file section found in the input.
/// An empty or whitespace-only input returns an empty `Vec`.
pub fn split_by_file(diff: &str) -> Vec<FileDiff> {
    let mut result = Vec::new();
    let mut positions = Vec::new();

    // Find all positions where a file section starts (at line boundaries).
    if diff.starts_with(FILE_DIFF_MARKER) {
        positions.push(0);
    }
    let search = format!("\n{FILE_DIFF_MARKER}");
    let mut start = 0;
    while let Some(pos) = diff[start..].find(&search) {
        // +1 to skip the newline; the section starts at `diff`.
        positions.push(start + pos + 1);
        start = start + pos + 1;
    }

    for (i, &pos) in positions.iter().enumerate() {
        let end = positions.get(i + 1).copied().unwrap_or(diff.len());
        let content = &diff[pos..end];
        let first_line = content.lines().next().unwrap_or("");
        let path = extract_path_from_diff_header(first_line);

        result.push(FileDiff {
            path,
            content: content.to_string(),
            byte_len: content.len(),
        });
    }

    result
}

/// Splits a [`FileDiff`] into per-hunk segments.
///
/// Each [`HunkDiff`] includes the file header so it is self-contained.
/// A file with no hunks (e.g., binary files, mode-only changes) returns
/// an empty `Vec`.
pub fn split_file_by_hunk(file_diff: &FileDiff) -> Vec<HunkDiff> {
    let content = &file_diff.content;
    let mut hunk_positions = Vec::new();

    // Find all positions where a hunk starts (at line boundaries).
    if content.starts_with(HUNK_MARKER) {
        hunk_positions.push(0);
    }
    let search = format!("\n{HUNK_MARKER}");
    let mut start = 0;
    while let Some(pos) = content[start..].find(&search) {
        hunk_positions.push(start + pos + 1);
        start = start + pos + 1;
    }

    if hunk_positions.is_empty() {
        return Vec::new();
    }

    // Everything before the first hunk is the file header.
    let file_header = &content[..hunk_positions[0]];

    let mut result = Vec::new();
    for (i, &pos) in hunk_positions.iter().enumerate() {
        let end = hunk_positions.get(i + 1).copied().unwrap_or(content.len());
        let hunk_content = &content[pos..end];
        let byte_len = file_header.len() + hunk_content.len();

        result.push(HunkDiff {
            file_header: file_header.to_string(),
            content: hunk_content.to_string(),
            byte_len,
        });
    }

    result
}

/// Extracts the file path from the `b/` side of a `diff --git` header line.
fn extract_path_from_diff_header(header_line: &str) -> String {
    // Format: "diff --git a/old_path b/new_path"
    // Find the last " b/" to handle paths that may contain spaces.
    if let Some(b_pos) = header_line.rfind(" b/") {
        header_line[b_pos + 3..].to_string()
    } else {
        // Fallback: try to extract from after "diff --git a/".
        header_line
            .strip_prefix(FILE_DIFF_MARKER)
            .unwrap_or(header_line)
            .to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── test helpers ────────────────────────────────────────────

    /// Builds a standard single-file diff header.
    fn make_file_header(path: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\n\
             index abc1234..def5678 100644\n\
             --- a/{path}\n\
             +++ b/{path}\n"
        )
    }

    /// Builds a single hunk string.
    fn make_hunk(
        old_start: usize,
        old_count: usize,
        new_start: usize,
        new_count: usize,
        body: &str,
    ) -> String {
        format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@\n{body}")
    }

    /// Builds a complete single-file, single-hunk diff.
    fn make_single_file_diff(path: &str, hunk_body: &str) -> String {
        format!(
            "{}{}",
            make_file_header(path),
            make_hunk(1, 3, 1, 4, hunk_body)
        )
    }

    // ── split_by_file ──────────────────────────────────────────

    #[test]
    fn split_by_file_empty_input() {
        let result = split_by_file("");
        assert!(result.is_empty());
    }

    #[test]
    fn split_by_file_whitespace_only() {
        let result = split_by_file("   \n\n  \t  ");
        assert!(result.is_empty());
    }

    #[test]
    fn split_by_file_no_diff_markers() {
        let result = split_by_file("some random text\nwithout diff markers\n");
        assert!(result.is_empty());
    }

    #[test]
    fn split_by_file_single_file_single_hunk() {
        let diff = make_single_file_diff(
            "src/main.rs",
            " fn main() {\n+    println!(\"hello\");\n }\n",
        );
        let result = split_by_file(&diff);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "src/main.rs");
        assert_eq!(result[0].content, diff);
    }

    #[test]
    fn split_by_file_single_file_multiple_hunks() {
        let header = make_file_header("lib.rs");
        let hunk1 = make_hunk(1, 3, 1, 4, "+use std::io;\n");
        let hunk2 = make_hunk(10, 2, 11, 3, "+// new comment\n");
        let diff = format!("{header}{hunk1}{hunk2}");

        let result = split_by_file(&diff);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "lib.rs");
        assert!(result[0].content.contains("@@ -1,3 +1,4 @@"));
        assert!(result[0].content.contains("@@ -10,2 +11,3 @@"));
    }

    #[test]
    fn split_by_file_multiple_files() {
        let file1 = make_single_file_diff("a.rs", "+line\n");
        let file2 = make_single_file_diff("b.rs", "+other\n");
        let file3 = make_single_file_diff("c.rs", "+third\n");
        let diff = format!("{file1}{file2}{file3}");

        let result = split_by_file(&diff);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].path, "a.rs");
        assert_eq!(result[1].path, "b.rs");
        assert_eq!(result[2].path, "c.rs");
    }

    #[test]
    fn split_by_file_binary_marker() {
        let diff = "diff --git a/image.png b/image.png\n\
                     new file mode 100644\n\
                     index 0000000..abc1234\n\
                     Binary files /dev/null and b/image.png differ\n";

        let result = split_by_file(diff);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "image.png");
        assert!(result[0].content.contains("Binary files"));
    }

    #[test]
    fn split_by_file_rename() {
        let diff = "diff --git a/old_name.rs b/new_name.rs\n\
                     similarity index 95%\n\
                     rename from old_name.rs\n\
                     rename to new_name.rs\n\
                     index abc1234..def5678 100644\n\
                     --- a/old_name.rs\n\
                     +++ b/new_name.rs\n\
                     @@ -1,3 +1,3 @@\n\
                     -// old\n\
                     +// new\n";

        let result = split_by_file(diff);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "new_name.rs");
    }

    #[test]
    fn split_by_file_byte_len_matches_content() {
        let file1 = make_single_file_diff("a.rs", "+line\n");
        let file2 = make_single_file_diff("b.rs", "+other\n");
        let diff = format!("{file1}{file2}");

        let result = split_by_file(&diff);
        for file_diff in &result {
            assert_eq!(file_diff.byte_len, file_diff.content.len());
        }
    }

    #[test]
    fn split_by_file_content_preserved_verbatim() {
        let file1 = make_single_file_diff("a.rs", "+line\n");
        let file2 = make_single_file_diff("b.rs", "+other\n");
        let diff = format!("{file1}{file2}");

        let result = split_by_file(&diff);
        let rejoined: String = result.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(rejoined, diff);
    }

    // ── split_file_by_hunk ─────────────────────────────────────

    #[test]
    fn split_file_by_hunk_no_hunks() {
        let file_diff = FileDiff {
            path: "image.png".to_string(),
            content: "diff --git a/image.png b/image.png\n\
                      new file mode 100644\n\
                      index 0000000..abc1234\n\
                      Binary files /dev/null and b/image.png differ\n"
                .to_string(),
            byte_len: 0, // not relevant for this test
        };
        let result = split_file_by_hunk(&file_diff);
        assert!(result.is_empty());
    }

    #[test]
    fn split_file_by_hunk_single_hunk() {
        let content =
            make_single_file_diff("main.rs", " fn main() {\n+    println!(\"hi\");\n }\n");
        let file_diff = FileDiff {
            path: "main.rs".to_string(),
            byte_len: content.len(),
            content,
        };

        let result = split_file_by_hunk(&file_diff);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.starts_with(HUNK_MARKER));
        assert!(result[0].file_header.starts_with("diff --git"));
    }

    #[test]
    fn split_file_by_hunk_multiple_hunks() {
        let header = make_file_header("lib.rs");
        let hunk1 = make_hunk(1, 3, 1, 4, "+use std::io;\n");
        let hunk2 = make_hunk(10, 2, 11, 3, "+// comment\n");
        let hunk3 = make_hunk(20, 1, 22, 2, "+fn new() {}\n");
        let content = format!("{header}{hunk1}{hunk2}{hunk3}");
        let file_diff = FileDiff {
            path: "lib.rs".to_string(),
            byte_len: content.len(),
            content,
        };

        let result = split_file_by_hunk(&file_diff);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn split_file_by_hunk_header_included() {
        let header = make_file_header("lib.rs");
        let hunk1 = make_hunk(1, 3, 1, 4, "+line\n");
        let hunk2 = make_hunk(10, 2, 11, 3, "+other\n");
        let content = format!("{header}{hunk1}{hunk2}");
        let file_diff = FileDiff {
            path: "lib.rs".to_string(),
            byte_len: content.len(),
            content,
        };

        let result = split_file_by_hunk(&file_diff);
        for hunk in &result {
            assert!(
                hunk.file_header.contains("diff --git"),
                "file_header should contain the diff --git line"
            );
            assert!(
                hunk.file_header.contains("--- a/"),
                "file_header should contain the --- line"
            );
            assert!(
                hunk.file_header.contains("+++ b/"),
                "file_header should contain the +++ line"
            );
        }
    }

    #[test]
    fn split_file_by_hunk_content_starts_with_at() {
        let header = make_file_header("lib.rs");
        let hunk1 = make_hunk(1, 3, 1, 4, "+line\n");
        let hunk2 = make_hunk(10, 2, 11, 3, "+other\n");
        let content = format!("{header}{hunk1}{hunk2}");
        let file_diff = FileDiff {
            path: "lib.rs".to_string(),
            byte_len: content.len(),
            content,
        };

        let result = split_file_by_hunk(&file_diff);
        for hunk in &result {
            assert!(
                hunk.content.starts_with(HUNK_MARKER),
                "hunk content should start with '{}', got: {:?}",
                HUNK_MARKER,
                &hunk.content[..hunk.content.len().min(20)]
            );
        }
    }

    #[test]
    fn split_file_by_hunk_byte_len_is_header_plus_content() {
        let header = make_file_header("lib.rs");
        let hunk1 = make_hunk(1, 3, 1, 4, "+line\n");
        let hunk2 = make_hunk(10, 2, 11, 3, "+other\n");
        let content = format!("{header}{hunk1}{hunk2}");
        let file_diff = FileDiff {
            path: "lib.rs".to_string(),
            byte_len: content.len(),
            content,
        };

        let result = split_file_by_hunk(&file_diff);
        for hunk in &result {
            assert_eq!(
                hunk.byte_len,
                hunk.file_header.len() + hunk.content.len(),
                "byte_len should equal file_header.len() + content.len()"
            );
        }
    }

    #[test]
    fn split_file_by_hunk_mode_change_only() {
        let content = "diff --git a/script.sh b/script.sh\n\
                        old mode 100644\n\
                        new mode 100755\n"
            .to_string();
        let file_diff = FileDiff {
            path: "script.sh".to_string(),
            byte_len: content.len(),
            content,
        };

        let result = split_file_by_hunk(&file_diff);
        assert!(result.is_empty());
    }

    // ── extract_path_from_diff_header ──────────────────────────

    #[test]
    fn path_extraction_simple() {
        assert_eq!(
            extract_path_from_diff_header("diff --git a/foo.rs b/foo.rs"),
            "foo.rs"
        );
    }

    #[test]
    fn path_extraction_nested() {
        assert_eq!(
            extract_path_from_diff_header("diff --git a/src/git/diff.rs b/src/git/diff.rs"),
            "src/git/diff.rs"
        );
    }

    #[test]
    fn path_extraction_rename() {
        assert_eq!(
            extract_path_from_diff_header("diff --git a/old.rs b/new.rs"),
            "new.rs"
        );
    }

    #[test]
    fn path_extraction_with_spaces() {
        assert_eq!(
            extract_path_from_diff_header("diff --git a/my file.rs b/my file.rs"),
            "my file.rs"
        );
    }

    // ── roundtrip ──────────────────────────────────────────────

    #[test]
    fn roundtrip_split_and_rejoin() {
        let file1 = make_single_file_diff("a.rs", "+line1\n");
        let file2 = make_single_file_diff("b.rs", "+line2\n");
        let file3 = make_single_file_diff("c.rs", "+line3\n");
        let original = format!("{file1}{file2}{file3}");

        let files = split_by_file(&original);
        let rejoined: String = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(rejoined, original);
    }
}
