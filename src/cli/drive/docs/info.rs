//! CLI command for `omni-dev drive docs info`.

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::cli::drive::format::{
    output_as, sanitize_for_terminal, write_scalar_jsonl, JsonlSerialize, OutputFormat,
};
use crate::drive::client::DriveClient;
use crate::drive::docs::api::DocsApi;
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::structure::{outline, TabOutline};
use crate::drive::docs::target;
use crate::drive::docs::types::Document;
use crate::drive::files_api::FilesApi;

/// Shows a document's title, revision id and structural outline.
#[derive(Parser)]
pub struct InfoCommand {
    /// Document id (the `/d/<ID>/` segment of a Docs URL).
    pub document_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// What `docs info` reports.
///
/// A CLI-layer aggregate rather than an engine type: it composes the
/// engine's per-tab [`TabOutline`]s with the document's identity purely for
/// presentation, and nothing in `crate::drive` needs the combination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfoOutcome {
    /// The document's id.
    pub document_id: String,
    /// Its title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Its revision at the moment of the read.
    ///
    /// The most important thing this command reports: it is the
    /// `writeControl.requiredRevisionId` token a `documents.batchUpdate`
    /// presents so a write against a document that moved underneath it is
    /// refused rather than misapplied. Nothing else in the CLI surfaces it.
    /// Absent when the caller lacks edit access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// Named-range names and how many ranges carry each, in sorted order.
    ///
    /// Named ranges are the *stable* way to address a region — an index
    /// shifts on every insertion, a name does not — so they earn a place in
    /// "info" rather than being an advanced-usage footnote.
    pub named_ranges: Vec<NamedRangeSummary>,
    /// One entry per tab, in document order.
    pub tabs: Vec<TabOutline>,
}

/// One named range group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NamedRangeSummary {
    /// The shared name.
    pub name: String,
    /// How many ranges carry it.
    pub ranges: usize,
}

impl JsonlSerialize for InfoOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

impl InfoOutcome {
    /// Builds the outcome from a fetched document.
    #[must_use]
    pub fn of(document_id: &str, document: &Document) -> Self {
        let mut named_ranges: Vec<NamedRangeSummary> = document
            .named_ranges
            .iter()
            .map(|(name, group)| NamedRangeSummary {
                name: name.clone(),
                ranges: group.named_ranges.len(),
            })
            .collect();
        // Sorted, because `HashMap` iteration order is arbitrary and an
        // "info" command whose output reorders between identical runs is
        // useless for diffing.
        named_ranges.sort_by(|a, b| a.name.cmp(&b.name));

        Self {
            document_id: document_id.to_string(),
            title: document.title.clone(),
            revision_id: document.revision_id.clone(),
            named_ranges,
            tabs: document.resolved_tabs().iter().map(outline).collect(),
        }
    }
}

impl InfoCommand {
    /// Runs the command, deriving a Docs client from the shared Drive one.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        run_info(client, &docs, &self.document_id, &self.output).await
    }
}

/// Fetches the document and renders its outline.
///
/// Split from [`InfoCommand::execute`] so tests can inject wiremock clients
/// without going through the credential-loading path.
async fn run_info(
    drive: &DriveClient,
    docs: &DocsClient,
    document_id: &str,
    output: &OutputFormat,
) -> Result<()> {
    let document = match DocsApi::new(docs)
        .get_document(
            document_id,
            crate::drive::docs::api::SuggestionsViewMode::default(),
        )
        .await
    {
        Ok(document) => document,
        // Only now is a `files.get` worth spending — see
        // `target::explain_failure` for why classification is lazy.
        Err(err) => {
            return Err(
                target::explain_failure(&FilesApi::new(drive), document_id, "info", err).await,
            )
        }
    };
    let outcome = InfoOutcome::of(document_id, &document);
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_info_table(&outcome, &mut handle)
}

/// Renders a header block plus one block per tab — a "table" in the sense of
/// "one command, one rendering", matching `sheets info`'s precedent.
///
/// Titles and heading text are *chrome* here rather than content, so they go
/// through `sanitize_for_terminal`, exactly as `sheets info` sanitizes sheet
/// titles.
fn render_info_table(outcome: &InfoOutcome, out: &mut dyn std::io::Write) -> Result<()> {
    let ctx = "Failed to write docs info";
    writeln!(out, "Id: {}", sanitize_for_terminal(&outcome.document_id)).context(ctx)?;
    writeln!(
        out,
        "Title: {}",
        sanitize_for_terminal(outcome.title.as_deref().unwrap_or(""))
    )
    .context(ctx)?;
    match &outcome.revision_id {
        Some(revision) => {
            writeln!(out, "Revision: {}", sanitize_for_terminal(revision)).context(ctx)?;
        }
        // Say why it is missing rather than omitting the line: its absence
        // is exactly what will make a later `docs replace` refuse, and
        // "read-only access" is the answer to that.
        None => writeln!(out, "Revision: (none — read-only access)").context(ctx)?,
    }
    writeln!(out, "Tabs: {}", outcome.tabs.len()).context(ctx)?;

    if !outcome.named_ranges.is_empty() {
        writeln!(out, "Named ranges: {}", outcome.named_ranges.len()).context(ctx)?;
        for range in &outcome.named_ranges {
            writeln!(
                out,
                "  {} ({} range(s))",
                sanitize_for_terminal(&range.name),
                range.ranges
            )
            .context(ctx)?;
        }
    }

    for tab in &outcome.tabs {
        writeln!(out).context(ctx)?;
        let label = match (&tab.tab_id, &tab.title) {
            (Some(id), Some(title)) => format!(
                "Tab: {} \"{}\"",
                sanitize_for_terminal(id),
                sanitize_for_terminal(title)
            ),
            (Some(id), None) => format!("Tab: {}", sanitize_for_terminal(id)),
            _ => "Tab: (single body)".to_string(),
        };
        let mut counts = vec![
            format!("{} chars", tab.characters),
            format!("{} paragraphs", tab.paragraphs),
        ];
        if tab.tables > 0 {
            counts.push(format!("{} tables", tab.tables));
        }
        if tab.section_breaks > 0 {
            counts.push(format!("{} section breaks", tab.section_breaks));
        }
        if tab.tables_of_contents > 0 {
            counts.push(format!("{} TOCs", tab.tables_of_contents));
        }
        writeln!(out, "{label} — {}", counts.join(", ")).context(ctx)?;

        for heading in &tab.headings {
            writeln!(
                out,
                "  {:<10} [{}..{})  {}",
                heading.style,
                heading.start_index,
                heading.end_index,
                sanitize_for_terminal(&heading.text)
            )
            .context(ctx)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn document(json: serde_json::Value) -> Document {
        serde_json::from_value(json).unwrap()
    }

    fn paragraph(start: i64, end: i64, text: &str, style: &str) -> serde_json::Value {
        serde_json::json!({
            "startIndex": start, "endIndex": end,
            "paragraph": {
                "elements": [{"textRun": {"content": text}}],
                "paragraphStyle": {"namedStyleType": style},
            },
        })
    }

    fn render(outcome: &InfoOutcome) -> String {
        let mut buf = Vec::new();
        render_info_table(outcome, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn reports_id_title_and_revision() {
        let doc = document(serde_json::json!({
            "documentId": "d1", "title": "Design Doc", "revisionId": "rev-abc",
            "body": {"content": []},
        }));
        let text = render(&InfoOutcome::of("d1", &doc));
        assert!(text.contains("Id: d1"), "{text}");
        assert!(text.contains("Title: Design Doc"), "{text}");
        assert!(text.contains("Revision: rev-abc"), "{text}");
    }

    /// A missing revision id is what will make a later `docs replace`
    /// refuse, so the reason is stated rather than the line omitted.
    #[test]
    fn a_missing_revision_id_explains_itself() {
        let doc = document(serde_json::json!({"documentId": "d1", "body": {"content": []}}));
        let text = render(&InfoOutcome::of("d1", &doc));
        assert!(
            text.contains("Revision: (none — read-only access)"),
            "{text}"
        );
    }

    #[test]
    fn lists_every_tab_with_its_counts() {
        let doc = document(serde_json::json!({
            "documentId": "d1", "title": "T",
            "tabs": [
                {
                    "tabProperties": {"tabId": "t.0", "title": "Overview"},
                    "documentTab": {"body": {"content": [
                        paragraph(1, 10, "Overview\n", "HEADING_1"),
                        paragraph(10, 30, "prose\n", "NORMAL_TEXT"),
                    ]}},
                },
                {
                    "tabProperties": {"tabId": "t.1", "title": "Appendix"},
                    "documentTab": {"body": {"content": [paragraph(1, 12, "x\n", "NORMAL_TEXT")]}},
                },
            ],
        }));
        let text = render(&InfoOutcome::of("d1", &doc));
        assert!(text.contains("Tabs: 2"), "{text}");
        assert!(text.contains("Tab: t.0 \"Overview\""), "{text}");
        assert!(text.contains("2 paragraphs"), "{text}");
        assert!(text.contains("30 chars"), "{text}");
        assert!(text.contains("Tab: t.1 \"Appendix\""), "{text}");
    }

    #[test]
    fn lists_headings_with_their_index_ranges() {
        let doc = document(serde_json::json!({
            "documentId": "d1",
            "body": {"content": [
                paragraph(1, 10, "Overview\n", "HEADING_1"),
                paragraph(10, 20, "prose\n", "NORMAL_TEXT"),
                paragraph(20, 30, "Goals\n", "HEADING_2"),
            ]},
        }));
        let text = render(&InfoOutcome::of("d1", &doc));
        assert!(text.contains("HEADING_1  [1..10)  Overview"), "{text}");
        assert!(text.contains("HEADING_2  [20..30)  Goals"), "{text}");
        assert!(!text.contains("prose"), "{text}");
    }

    #[test]
    fn lists_named_ranges_sorted_by_name() {
        let doc = document(serde_json::json!({
            "documentId": "d1", "body": {"content": []},
            "namedRanges": {
                "zeta": {"name": "zeta", "namedRanges": [{"name": "zeta", "ranges": []}]},
                "alpha": {"name": "alpha", "namedRanges": [
                    {"name": "alpha", "ranges": []}, {"name": "alpha", "ranges": []}]},
            },
        }));
        let outcome = InfoOutcome::of("d1", &doc);
        // Sorted, because HashMap order is arbitrary and an info command
        // that reorders between identical runs cannot be diffed.
        assert_eq!(
            outcome
                .named_ranges
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        let text = render(&outcome);
        assert!(text.contains("Named ranges: 2"), "{text}");
        assert!(text.contains("alpha (2 range(s))"), "{text}");
    }

    #[test]
    fn omits_the_named_ranges_block_when_there_are_none() {
        let doc = document(serde_json::json!({"documentId": "d1", "body": {"content": []}}));
        assert!(!render(&InfoOutcome::of("d1", &doc)).contains("Named ranges"));
    }

    /// Titles and heading text are server-controlled chrome here.
    #[test]
    fn strips_control_bytes_from_server_strings() {
        let doc = document(serde_json::json!({
            "documentId": "d1", "title": "T\u{1b}[31mitle",
            "body": {"content": [paragraph(1, 10, "He\nad\n", "HEADING_1")]},
        }));
        let text = render(&InfoOutcome::of("d1", &doc));
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(text.contains("Title: T[31mitle"), "{text}");
    }

    #[test]
    fn handles_a_document_with_no_body_and_no_tabs() {
        let doc = document(serde_json::json!({"documentId": "d1", "title": "Empty"}));
        let text = render(&InfoOutcome::of("d1", &doc));
        assert!(text.contains("Tabs: 0"), "{text}");
    }

    #[test]
    fn serialises_with_absent_optionals_omitted() {
        let doc = document(serde_json::json!({"documentId": "d1", "body": {"content": []}}));
        let json = serde_json::to_value(InfoOutcome::of("d1", &doc)).unwrap();
        assert!(json.get("title").is_none());
        assert!(json.get("revision_id").is_none());
        assert!(json["tabs"].is_array());
    }
}
