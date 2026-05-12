//! Embedded reference resources shared by the CLI and the MCP server.
//!
//! Resources are exposed via two surfaces that must stay in lock step:
//! - CLI: `omni-dev resources show <id>` / `omni-dev resources list`
//! - MCP: `omni-dev://<id>` (e.g. `omni-dev://specs/jfm`)
//!
//! Content is embedded into the binary at compile time so installed builds
//! (`cargo install omni-dev`) serve it without reading from disk.

/// JFM (JIRA-Flavoured Markdown) specification, embedded from
/// `docs/specs/jfm.md`.
pub const SPEC_JFM: &str = include_str!("../docs/specs/jfm.md");

/// One entry in the embedded-resource registry.
pub struct Resource {
    /// Canonical, path-style id (e.g. `"specs/jfm"`).
    pub id: &'static str,
    /// Raw embedded content.
    pub content: &'static str,
    /// MIME type advertised to MCP clients.
    pub mime_type: &'static str,
}

/// The complete static registry. Kept lexicographically sorted by `id` so
/// `list` output stays deterministic as entries are added.
pub const REGISTRY: &[Resource] = &[Resource {
    id: "specs/jfm",
    content: SPEC_JFM,
    mime_type: "text/markdown",
}];

/// Returns the resource with the given canonical id, or `None`.
///
/// Lookup is exact-string and case-sensitive. The `omni-dev://` URI scheme
/// must be stripped by the caller before lookup.
pub fn get(id: &str) -> Option<&'static Resource> {
    REGISTRY.iter().find(|r| r.id == id)
}

/// All ids in registry order. Used for `list` output and error messages.
pub fn ids() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|r| r.id)
}

/// Comma-separated list of known ids, for error messages.
pub fn known_ids_csv() -> String {
    ids().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn spec_jfm_is_embedded_and_non_empty() {
        assert!(!SPEC_JFM.is_empty());
        // Guard against the file moving or being emptied: the heading is
        // load-bearing for clients that match on it.
        assert!(
            SPEC_JFM.contains("# JFM (JIRA-Flavored Markdown) Specification"),
            "JFM spec missing expected heading"
        );
    }

    #[test]
    fn get_specs_jfm_returns_resource() {
        let r = get("specs/jfm").expect("specs/jfm must be registered");
        assert_eq!(r.id, "specs/jfm");
        assert_eq!(r.mime_type, "text/markdown");
        assert_eq!(r.content, SPEC_JFM);
    }

    #[test]
    fn get_unknown_returns_none() {
        assert!(get("specs/bogus").is_none());
        assert!(get("").is_none());
        // The old short-form id ("jfm") was replaced by the path-style id;
        // make sure it no longer resolves.
        assert!(get("jfm").is_none());
    }

    #[test]
    fn get_is_case_sensitive() {
        assert!(get("Specs/Jfm").is_none());
        assert!(get("SPECS/JFM").is_none());
    }

    #[test]
    fn ids_yields_registered_entries() {
        let collected: Vec<&str> = ids().collect();
        assert!(collected.contains(&"specs/jfm"));
    }

    #[test]
    fn known_ids_csv_contains_specs_jfm() {
        assert!(known_ids_csv().contains("specs/jfm"));
    }
}
