//! Shared create-from-frontmatter resolution for the CLI and MCP create paths.
//!
//! Both `omni-dev atlassian {jira,confluence} create` and the MCP
//! `jira_create` / `confluence_create` tools resolve a JFM document
//! (frontmatter + body) plus explicit overrides into the fields needed to
//! create an issue/page. Keeping that logic here — instead of duplicating it
//! per surface — is what lets the read → edit → create round-trip behave
//! identically on the CLI and over MCP.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::atlassian::adf_validated::{markdown_to_validated_adf, ValidatedAdfDocument};
use crate::atlassian::client::{AtlassianClient, JiraCreatedIssue};
use crate::atlassian::custom_fields::{merge_set_field_overrides, resolve_custom_fields};
use crate::atlassian::document::{
    split_custom_sections, split_frontmatter, CustomFieldSection, JfmDocument, JfmFrontmatter,
    JiraCreateFrontmatter,
};

// ── override warnings ───────────────────────────────────────────────────────

/// A frontmatter field that was present but shadowed by an explicit
/// flag/parameter override. Recorded so callers can warn the user that their
/// frontmatter value was ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedField {
    /// Field name as it appears in frontmatter (e.g. `project`, `space_key`).
    pub field: &'static str,
    /// The value present in the frontmatter.
    pub frontmatter_value: String,
    /// The overriding value supplied via flag/parameter.
    pub override_value: String,
}

impl ShadowedField {
    /// One-line warning, suitable for stderr (CLI) or in-band prepending (MCP,
    /// where stderr is not visible to the caller).
    pub fn warning_line(&self) -> String {
        format!(
            "warning: frontmatter `{}` (\"{}\") was overridden by an explicit `{}` argument (\"{}\")",
            self.field, self.frontmatter_value, self.field, self.override_value
        )
    }
}

/// Applies explicit-override-over-frontmatter precedence, recording a
/// [`ShadowedField`] when a non-empty frontmatter value is replaced by a
/// *differing* override (equal values are kept silently — no noise).
fn resolve_override(
    field: &'static str,
    override_value: Option<&str>,
    frontmatter_value: Option<String>,
    shadowed: &mut Vec<ShadowedField>,
) -> Option<String> {
    match (override_value, frontmatter_value) {
        (Some(ov), Some(fm)) => {
            if ov != fm {
                shadowed.push(ShadowedField {
                    field,
                    frontmatter_value: fm,
                    override_value: ov.to_string(),
                });
            }
            Some(ov.to_string())
        }
        (Some(ov), None) => Some(ov.to_string()),
        (None, fm) => fm,
    }
}

/// Prepends one warning line per shadowed field to `body`, for in-band (MCP)
/// surfacing. Returns `body` unchanged when nothing was shadowed.
pub fn prepend_warnings(shadowed: &[ShadowedField], body: String) -> String {
    if shadowed.is_empty() {
        return body;
    }
    let mut out = String::new();
    for s in shadowed {
        out.push_str(&s.warning_line());
        out.push('\n');
    }
    out.push_str(&body);
    out
}

// ── JIRA ────────────────────────────────────────────────────────────────────

/// Resolved inputs for creating a JIRA issue, plus any shadowed-field records.
#[derive(Debug)]
pub struct ResolvedJiraCreate {
    /// Project key.
    pub project: String,
    /// Issue type (defaults to `Task`).
    pub issue_type: String,
    /// Issue summary / title.
    pub summary: String,
    /// Labels applied to the issue.
    pub labels: Vec<String>,
    /// Validated ADF body.
    pub adf: ValidatedAdfDocument,
    /// Scalar custom field values keyed by human-readable name.
    pub custom_scalars: BTreeMap<String, serde_yaml::Value>,
    /// Rich-text custom field sections extracted from the body.
    pub custom_sections: Vec<CustomFieldSection>,
    /// Frontmatter values shadowed by an explicit override.
    pub shadowed: Vec<ShadowedField>,
}

/// Resolves a JFM `document` plus explicit overrides into JIRA create fields.
///
/// Frontmatter is optional — a body with no `---` block is treated entirely as
/// the issue body. Precedence is override → frontmatter → derived/default.
/// `set_field_overrides` are `NAME=VALUE` custom-field overrides (the CLI's
/// `--set-field`); the MCP passes an empty vec.
pub fn resolve_jira_create(
    document: &str,
    override_project: Option<&str>,
    override_summary: Option<&str>,
    override_issue_type: Option<&str>,
    set_field_overrides: Vec<(String, serde_yaml::Value)>,
) -> Result<ResolvedJiraCreate> {
    let (fm_yaml, raw_body) = split_frontmatter(document)?;

    let fm: JiraCreateFrontmatter = match fm_yaml {
        Some(yaml) => serde_yaml::from_str(yaml).context("Failed to parse JFM frontmatter YAML")?,
        None => JiraCreateFrontmatter::default(),
    };

    if fm.r#type.as_deref() == Some("confluence") {
        anyhow::bail!("Cannot create a JIRA issue from Confluence frontmatter");
    }

    let (body_md, custom_sections) = split_custom_sections(&raw_body);
    let adf = markdown_to_validated_adf(&body_md)?;

    let mut shadowed = Vec::new();

    // Derive project from `key:` when no explicit `project:` is present.
    let fm_project = fm.project.clone().or_else(|| {
        if fm.key.is_empty() {
            None
        } else {
            fm.key.split('-').next().map(String::from)
        }
    });
    let project = resolve_override("project", override_project, fm_project, &mut shadowed)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Project key is required: set `project:` or `key:` in the frontmatter, or pass an \
                 explicit project"
            )
        })?;

    let fm_summary = fm.summary.clone().filter(|s| !s.is_empty());
    let summary =
        resolve_override("summary", override_summary, fm_summary, &mut shadowed).ok_or_else(
            || anyhow::anyhow!("Summary is required: set `summary:` in the frontmatter, or pass an explicit summary"),
        )?;

    let issue_type = resolve_override(
        "issue_type",
        override_issue_type,
        fm.issue_type,
        &mut shadowed,
    )
    .unwrap_or_else(|| "Task".to_string());

    let custom_scalars = merge_set_field_overrides(fm.custom_fields, set_field_overrides);

    Ok(ResolvedJiraCreate {
        project,
        issue_type,
        summary,
        labels: fm.labels,
        adf,
        custom_scalars,
        custom_sections,
        shadowed,
    })
}

/// Creates a JIRA issue from resolved fields.
///
/// Fast path when no custom fields are requested: one POST to
/// `/rest/api/3/issue`. With custom fields, fetches `createmeta` to resolve
/// human names to IDs and dispatch values by schema before sending.
#[allow(clippy::too_many_arguments)]
pub async fn create_resolved_jira_issue(
    client: &AtlassianClient,
    project: &str,
    issue_type: &str,
    summary: &str,
    adf: &ValidatedAdfDocument,
    labels: &[String],
    custom_scalars: &BTreeMap<String, serde_yaml::Value>,
    custom_sections: &[CustomFieldSection],
) -> Result<JiraCreatedIssue> {
    let custom_fields = if custom_scalars.is_empty() && custom_sections.is_empty() {
        BTreeMap::new()
    } else {
        let createmeta = client.get_createmeta(project, issue_type).await?;
        resolve_custom_fields(custom_scalars, custom_sections, &createmeta)?
    };

    client
        .create_issue_with_custom_fields(
            project,
            issue_type,
            summary,
            Some(adf),
            labels,
            &custom_fields,
        )
        .await
}

// ── Confluence ──────────────────────────────────────────────────────────────

/// Resolved inputs for creating a Confluence page, plus shadowed-field records.
#[derive(Debug)]
pub struct ResolvedConfluenceCreate {
    /// Target space key.
    pub space_key: String,
    /// Page title.
    pub title: String,
    /// Optional parent page ID.
    pub parent_id: Option<String>,
    /// Validated ADF body.
    pub adf: ValidatedAdfDocument,
    /// Frontmatter values shadowed by an explicit override.
    pub shadowed: Vec<ShadowedField>,
}

/// Resolves a JFM `document` plus explicit overrides into Confluence create fields.
///
/// Expects the strict form `confluence_read` emits. Precedence is
/// override → frontmatter.
pub fn resolve_confluence_create(
    document: &str,
    override_space: Option<&str>,
    override_title: Option<&str>,
    override_parent: Option<&str>,
) -> Result<ResolvedConfluenceCreate> {
    let doc = JfmDocument::parse(document)?;

    let fm = match &doc.frontmatter {
        JfmFrontmatter::Confluence(fm) => fm,
        JfmFrontmatter::Jira(_) => {
            anyhow::bail!("Cannot create a Confluence page from JIRA frontmatter");
        }
    };

    let adf = markdown_to_validated_adf(&doc.body)?;

    let mut shadowed = Vec::new();

    let fm_space = (!fm.space_key.is_empty()).then(|| fm.space_key.clone());
    let space_key = resolve_override("space_key", override_space, fm_space, &mut shadowed)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Space key is required: set `space_key:` in the frontmatter, or pass an explicit \
                 space"
            )
        })?;

    let fm_title = (!fm.title.is_empty()).then(|| fm.title.clone());
    let title =
        resolve_override("title", override_title, fm_title, &mut shadowed).ok_or_else(|| {
            anyhow::anyhow!(
                "Title is required: set `title:` in the frontmatter, or pass an explicit title"
            )
        })?;

    let parent_id = resolve_override(
        "parent_id",
        override_parent,
        fm.parent_id.clone(),
        &mut shadowed,
    );

    Ok(ResolvedConfluenceCreate {
        space_key,
        title,
        parent_id,
        adf,
        shadowed,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const JIRA_DOC: &str =
        "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-123\nsummary: Title\n---\n\nBody\n";

    #[test]
    fn jira_derives_project_from_key() {
        let r = resolve_jira_create(JIRA_DOC, None, None, None, vec![]).unwrap();
        assert_eq!(r.project, "PROJ");
        assert_eq!(r.summary, "Title");
        assert_eq!(r.issue_type, "Task");
        assert!(r.shadowed.is_empty());
    }

    #[test]
    fn jira_explicit_project_field_wins_over_key() {
        let doc = "---\ntype: jira\nkey: ABC-1\nproject: XYZ\nsummary: T\n---\n\nB\n";
        let r = resolve_jira_create(doc, None, None, None, vec![]).unwrap();
        assert_eq!(r.project, "XYZ");
    }

    #[test]
    fn jira_override_records_shadowed() {
        let r = resolve_jira_create(JIRA_DOC, Some("NEW"), None, None, vec![]).unwrap();
        assert_eq!(r.project, "NEW");
        assert_eq!(r.shadowed.len(), 1);
        assert_eq!(r.shadowed[0].field, "project");
        assert_eq!(r.shadowed[0].frontmatter_value, "PROJ");
        assert_eq!(r.shadowed[0].override_value, "NEW");
        assert!(r.shadowed[0].warning_line().contains("PROJ"));
        assert!(r.shadowed[0].warning_line().contains("NEW"));
    }

    #[test]
    fn jira_equal_override_is_not_shadowed() {
        let r = resolve_jira_create(JIRA_DOC, Some("PROJ"), None, None, vec![]).unwrap();
        assert_eq!(r.project, "PROJ");
        assert!(r.shadowed.is_empty());
    }

    #[test]
    fn jira_no_frontmatter_uses_overrides() {
        let r =
            resolve_jira_create("just a body\n", Some("PROJ"), Some("S"), None, vec![]).unwrap();
        assert_eq!(r.project, "PROJ");
        assert_eq!(r.summary, "S");
        assert!(r.shadowed.is_empty());
    }

    #[test]
    fn jira_missing_project_errors() {
        let doc = "---\ntype: jira\nsummary: No project\n---\n\nB\n";
        let err = resolve_jira_create(doc, None, None, None, vec![]).unwrap_err();
        assert!(err.to_string().contains("Project key is required"));
    }

    #[test]
    fn jira_missing_summary_errors() {
        let doc = "---\ntype: jira\nproject: PROJ\n---\n\nB\n";
        let err = resolve_jira_create(doc, None, None, None, vec![]).unwrap_err();
        assert!(err.to_string().contains("Summary is required"));
    }

    #[test]
    fn jira_rejects_confluence_frontmatter() {
        let doc = "---\ntype: confluence\nspace_key: ENG\ntitle: T\n---\n\nB\n";
        let err = resolve_jira_create(doc, None, None, None, vec![]).unwrap_err();
        assert!(err.to_string().contains("Confluence"));
    }

    const CONF_DOC: &str =
        "---\ntype: confluence\ninstance: https://org.atlassian.net\npage_id: '7'\ntitle: Page\nspace_key: ENG\n---\n\nBody\n";

    #[test]
    fn confluence_resolves_from_frontmatter() {
        let r = resolve_confluence_create(CONF_DOC, None, None, None).unwrap();
        assert_eq!(r.space_key, "ENG");
        assert_eq!(r.title, "Page");
        assert!(r.shadowed.is_empty());
    }

    #[test]
    fn confluence_override_records_shadowed() {
        let r = resolve_confluence_create(CONF_DOC, Some("NEW"), None, None).unwrap();
        assert_eq!(r.space_key, "NEW");
        assert_eq!(r.shadowed.len(), 1);
        assert_eq!(r.shadowed[0].field, "space_key");
    }

    #[test]
    fn confluence_rejects_jira_frontmatter() {
        let doc = "---\ntype: jira\nkey: PROJ-1\nsummary: T\ninstance: https://o.net\n---\n\nB\n";
        let err = resolve_confluence_create(doc, None, None, None).unwrap_err();
        assert!(err.to_string().contains("JIRA"));
    }

    #[test]
    fn confluence_missing_space_errors() {
        // Empty `space_key:` with no override → space-required error.
        let doc =
            "---\ntype: confluence\ninstance: https://o.net\ntitle: T\nspace_key: ''\n---\n\nB\n";
        let err = resolve_confluence_create(doc, None, None, None).unwrap_err();
        assert!(err.to_string().contains("Space key is required"));
    }

    #[test]
    fn confluence_missing_title_errors() {
        // Space present, empty `title:` with no override → title-required error.
        let doc =
            "---\ntype: confluence\ninstance: https://o.net\ntitle: ''\nspace_key: ENG\n---\n\nB\n";
        let err = resolve_confluence_create(doc, None, None, None).unwrap_err();
        assert!(err.to_string().contains("Title is required"));
    }

    #[test]
    fn prepend_warnings_noop_when_empty() {
        assert_eq!(prepend_warnings(&[], "body".to_string()), "body");
    }
}
