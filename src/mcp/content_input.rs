//! Shared helper for MCP write-style tools that accept a body either inline
//! or as a filesystem path.
//!
//! AI callers pay an O(size) generation cost to emit a large body inline
//! through a tool call — the model has to write every byte through its output
//! stream. When the body is already on disk, a `*_path` parameter lets the
//! server read it directly, sidestepping that cost. This is the write-side
//! mirror of the read-side `output_file` parameter (see [`super::output_file`]).
//! See issue #1093 for the motivating case (a 70 KB Confluence page write that
//! took minutes to emit inline versus seconds from disk).

use anyhow::{Context, Result};

/// Resolves a body parameter supplied either inline or via a filesystem path.
///
/// Exactly one of `inline` / `path` may be set:
/// - `Ok(Some(body))` — one source was provided (a `path` is read from disk as
///   UTF-8),
/// - `Ok(None)` — neither was provided; the caller decides whether that is an
///   error, since some bodies are optional (e.g. `jira_write` may update only
///   the assignee),
/// - `Err` — both were provided (they are mutually exclusive), or the file
///   could not be read.
///
/// `field` names the inline parameter (e.g. `"content"`); the path parameter is
/// assumed to be `"{field}_path"` for error messages.
pub fn resolve_content_input(
    inline: Option<&str>,
    path: Option<&str>,
    field: &str,
) -> Result<Option<String>> {
    match (inline, path) {
        (Some(_), Some(_)) => {
            anyhow::bail!("Provide either `{field}` or `{field}_path`, not both.")
        }
        (Some(inline), None) => Ok(Some(inline.to_string())),
        (None, Some(path)) => {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read `{field}_path` file {path}"))?;
            Ok(Some(body))
        }
        (None, None) => Ok(None),
    }
}

/// Resolves mandatory inline/file UTF-8 content with a byte cap.
///
/// Used by Drive writes. File reads are bounded on the opened handle, so a
/// file growing after its size check cannot cause an unbounded allocation.
/// Special files are refused; on Unix nonblocking open also prevents a FIFO
/// replacement from waiting for a writer before its type can be checked.
pub(crate) fn require_bounded_content_input(
    inline: Option<&str>,
    path: Option<&str>,
    field: &str,
    cap: u64,
) -> Result<String> {
    use std::io::Read as _;

    match (inline, path) {
        (Some(_), Some(_)) => {
            anyhow::bail!("Provide either `{field}` or `{field}_path`, not both.")
        }
        (Some(text), None) => {
            anyhow::ensure!(
                text.len() as u64 <= cap,
                "`{field}` exceeds the {cap} byte cap"
            );
            Ok(text.to_owned())
        }
        (None, Some(path)) => {
            anyhow::ensure!(
                path != "-",
                "`{field}_path` must be a file; stdin is unsupported"
            );
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.custom_flags(nix::libc::O_NONBLOCK);
            }
            let file = options
                .open(path)
                .with_context(|| format!("Failed to open `{field}_path` file {path}"))?;
            let metadata = file
                .metadata()
                .with_context(|| format!("Failed to stat `{field}_path` file {path}"))?;
            anyhow::ensure!(
                metadata.is_file(),
                "`{field}_path` must be a regular UTF-8 file"
            );
            anyhow::ensure!(
                metadata.len() <= cap,
                "`{field}_path` exceeds the {cap} byte cap"
            );
            let mut bytes = Vec::new();
            file.take(cap.saturating_add(1))
                .read_to_end(&mut bytes)
                .with_context(|| format!("Failed to read `{field}_path` file {path}"))?;
            anyhow::ensure!(
                bytes.len() as u64 <= cap,
                "`{field}_path` exceeds the {cap} byte cap"
            );
            String::from_utf8(bytes)
                .with_context(|| format!("`{field}_path` file {path} is not valid UTF-8"))
        }
        (None, None) => anyhow::bail!("Provide either `{field}` or `{field}_path`."),
    }
}

/// Like [`resolve_content_input`] but for a body that must be present: returns
/// an error when neither `inline` nor `path` is supplied.
///
/// Used by tools whose body is mandatory (e.g. `confluence_write`,
/// `confluence_comment_add`).
pub fn require_content_input(
    inline: Option<&str>,
    path: Option<&str>,
    field: &str,
) -> Result<String> {
    resolve_content_input(inline, path, field)?
        .ok_or_else(|| anyhow::anyhow!("Provide either `{field}` or `{field}_path`."))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_returns_inline_when_only_inline_set() {
        let got = resolve_content_input(Some("hello"), None, "content").unwrap();
        assert_eq!(got.as_deref(), Some("hello"));
    }

    #[test]
    fn resolve_reads_file_when_only_path_set() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("body.md");
        std::fs::write(&path, "from disk").unwrap();

        let got = resolve_content_input(None, Some(path.to_str().unwrap()), "content").unwrap();
        assert_eq!(got.as_deref(), Some("from disk"));
    }

    #[test]
    fn resolve_returns_none_when_neither_set() {
        let got = resolve_content_input(None, None, "content").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn resolve_errors_when_both_set() {
        let err = resolve_content_input(Some("a"), Some("/tmp/x"), "content").unwrap_err();
        assert!(err
            .to_string()
            .contains("Provide either `content` or `content_path`, not both"));
    }

    #[test]
    fn resolve_errors_with_field_name_in_message() {
        let err = resolve_content_input(Some("a"), Some("/tmp/x"), "document").unwrap_err();
        assert!(err.to_string().contains("`document` or `document_path`"));
    }

    #[test]
    fn resolve_errors_when_path_missing() {
        let err =
            resolve_content_input(None, Some("/nonexistent_zxq/nope.md"), "content").unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to read `content_path` file"));
    }

    #[test]
    fn require_errors_when_neither_set() {
        let err = require_content_input(None, None, "content").unwrap_err();
        assert!(err
            .to_string()
            .contains("Provide either `content` or `content_path`."));
    }

    #[test]
    fn require_returns_body_when_path_set() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("body.md");
        std::fs::write(&path, "required body").unwrap();

        let got = require_content_input(None, Some(path.to_str().unwrap()), "content").unwrap();
        assert_eq!(got, "required body");
    }
    #[test]
    fn bounded_input_accepts_the_cap_and_rejects_larger_or_invalid_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "abcd").unwrap();
        assert_eq!(
            require_bounded_content_input(None, Some(path.to_str().unwrap()), "text", 4).unwrap(),
            "abcd"
        );
        assert_eq!(
            require_bounded_content_input(Some("abcd"), None, "text", 4).unwrap(),
            "abcd"
        );
        assert!(
            require_bounded_content_input(Some("abcde"), None, "text", 4)
                .unwrap_err()
                .to_string()
                .contains("cap")
        );
        assert!(
            require_bounded_content_input(None, Some(path.to_str().unwrap()), "text", 3)
                .unwrap_err()
                .to_string()
                .contains("cap")
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert!(
            require_bounded_content_input(None, Some(path.to_str().unwrap()), "text", 4)
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
        assert!(
            require_bounded_content_input(Some("x"), Some("missing"), "text", 4)
                .unwrap_err()
                .to_string()
                .contains("not both")
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_input_refuses_devices_and_fifos_without_reading_them() {
        assert!(
            require_bounded_content_input(None, Some("/dev/zero"), "text", 4)
                .unwrap_err()
                .to_string()
                .contains("regular")
        );
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(
            require_bounded_content_input(None, Some(fifo.to_str().unwrap()), "text", 4)
                .unwrap_err()
                .to_string()
                .contains("regular")
        );
    }
}
