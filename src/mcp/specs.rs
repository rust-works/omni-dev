//! Reference specifications exposed as MCP resources at
//! `omni-dev://specs/{name}`.
//!
//! The spec text is embedded into the binary at compile time so installed
//! builds (`cargo install omni-dev`) can serve it without reading from disk.
//! Each spec is the same file humans browse on GitHub, so the resource and
//! the human-readable docs cannot drift.

/// JFM (JIRA-Flavoured Markdown) specification, embedded from
/// `docs/specs/jfm.md`.
pub const SPEC_JFM: &str = include_str!("../../docs/specs/jfm.md");

/// Looks up a spec by name.
///
/// Returns `(content, mime_type)` for known specs. Unknown names return
/// `None`; the caller is expected to surface that as `resource_not_found`.
pub fn lookup(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "jfm" => Some((SPEC_JFM, "text/markdown")),
        _ => None,
    }
}

/// Comma-separated list of known spec names, for error messages.
pub const KNOWN_SPECS: &str = "jfm";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn jfm_spec_is_embedded_and_non_empty() {
        assert!(!SPEC_JFM.is_empty());
        // Guard against the file moving or being emptied: the heading is
        // load-bearing for clients that match on it.
        assert!(
            SPEC_JFM.contains("# JFM (JIRA-Flavored Markdown) Specification"),
            "JFM spec missing expected heading"
        );
    }

    #[test]
    fn lookup_jfm_returns_markdown() {
        let (content, mime) = lookup("jfm").expect("jfm spec must be registered");
        assert_eq!(mime, "text/markdown");
        assert!(content.contains("JFM"));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("bogus").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("JFM").is_none(), "lookup is case-sensitive");
    }

    #[test]
    fn known_specs_lists_jfm() {
        assert!(KNOWN_SPECS.contains("jfm"));
    }
}
