//! CLI command for `omni-dev drive docs read`.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::docs::api::{DocsApi, SuggestionsViewMode};
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::read::{read, ReadOptions, ReadOutcome};
use crate::drive::docs::target;
use crate::drive::files_api::FilesApi;

/// Reads a document's structural elements with their index ranges.
///
/// `drive read --content` is the *prose* channel — it exports a Doc to
/// markdown. This is the *model* channel: it reports each element's
/// `[start, end)` index range, which is the only way to learn an index, and
/// the document's `revisionId`. An export is a one-way rendering with no
/// path back to either.
#[derive(Parser)]
pub struct ReadCommand {
    /// Document id (the `/d/<ID>/` segment of a Docs URL).
    pub document_id: String,

    /// Restrict output to one tab id (see `drive docs info`).
    #[arg(long)]
    pub tab: Option<String>,

    /// Which suggestion view the text and indices are reported against.
    #[arg(long, value_enum, default_value_t = SuggestionsViewArg::Default)]
    pub suggestions_view: SuggestionsViewArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// `clap` mirror of the engine's [`SuggestionsViewMode`].
///
/// The same engine/CLI split `ValueRenderOption`/`RenderArg` and
/// `DriveOperation`/`OperationArg` use: the engine enum stays `clap`-free so
/// a non-CLI caller never depends on the argument parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SuggestionsViewArg {
    /// Whatever the caller's access level implies.
    Default,
    /// Suggestions shown inline, as tracked changes.
    Inline,
    /// The document as it would be with every suggestion accepted.
    Accepted,
    /// The document as it would be with every suggestion rejected.
    Without,
}

impl From<SuggestionsViewArg> for SuggestionsViewMode {
    fn from(arg: SuggestionsViewArg) -> Self {
        match arg {
            SuggestionsViewArg::Default => Self::DefaultForCurrentAccess,
            SuggestionsViewArg::Inline => Self::Inline,
            SuggestionsViewArg::Accepted => Self::PreviewAccepted,
            SuggestionsViewArg::Without => Self::PreviewWithoutSuggestions,
        }
    }
}

impl ReadCommand {
    /// Runs the command, deriving a Docs client from the shared Drive one.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let opts = ReadOptions {
            document_id: self.document_id,
            tab: self.tab,
            suggestions: self.suggestions_view.into(),
        };
        run_read(client, &docs, &opts, &self.output).await
    }
}

/// Reads the document and renders it.
///
/// Split from [`ReadCommand::execute`] so tests can inject wiremock clients
/// without going through the credential-loading path.
async fn run_read(
    drive: &DriveClient,
    docs: &DocsClient,
    opts: &ReadOptions,
    output: &OutputFormat,
) -> Result<()> {
    let outcome = match read(&DocsApi::new(docs), opts).await {
        Ok(outcome) => outcome,
        // Only now is a `files.get` worth spending — see
        // `target::explain_failure` for why classification is lazy.
        Err(err) => {
            return Err(target::explain_failure(
                &FilesApi::new(drive),
                &opts.document_id,
                "read",
                err,
            )
            .await)
        }
    };
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_read_table(&outcome, &mut handle)
}

/// Renders one line per element, index-first, indented by nesting depth.
///
/// Element text is passed through `sanitize_for_terminal`, which is a
/// deliberate divergence from `sheets read`'s verbatim CSV. CSV is an
/// interchange format that must round-trip, so stripping control characters
/// there would corrupt real data; this is an *orientation* view whose whole
/// value is column alignment, and a soft line break (`\v`, which really does
/// occur inside Docs paragraph text) or an escape sequence in
/// server-controlled text would destroy it. `-o json` is the unsanitised
/// channel and is the one to use for content.
fn render_read_table(outcome: &ReadOutcome, out: &mut dyn std::io::Write) -> Result<()> {
    let ctx = "Failed to write docs read output";
    let multi_tab = outcome.tabs.len() > 1;

    for (position, tab) in outcome.tabs.iter().enumerate() {
        if multi_tab {
            if position > 0 {
                writeln!(out).context(ctx)?;
            }
            let id = tab.tab_id.as_deref().unwrap_or("(no id)");
            let title = tab.title.as_deref().unwrap_or("");
            writeln!(
                out,
                "# {} {}",
                sanitize_for_terminal(id),
                sanitize_for_terminal(title)
            )
            .context(ctx)?;
        }

        if tab.elements.is_empty() {
            writeln!(out, "(empty)").context(ctx)?;
            continue;
        }

        // Widths come from the data so the columns line up for this
        // document rather than for a hypothetical worst case.
        let start_w = tab
            .elements
            .iter()
            .map(|e| e.start_index.to_string().len())
            .max()
            .unwrap_or(5)
            .max(5);
        let end_w = tab
            .elements
            .iter()
            .map(|e| e.end_index.to_string().len())
            .max()
            .unwrap_or(3)
            .max(3);
        let kind_w = tab
            .elements
            .iter()
            .map(|e| e.kind.as_str().len() + e.depth() * 2)
            .max()
            .unwrap_or(4)
            .max(4);
        let style_w = tab
            .elements
            .iter()
            .filter_map(|e| e.style.as_ref().map(String::len))
            .max()
            .unwrap_or(5)
            .max(5);

        writeln!(
            out,
            "{:>start_w$}  {:>end_w$}  {:<kind_w$}  {:<style_w$}  TEXT",
            "START", "END", "KIND", "STYLE"
        )
        .context(ctx)?;

        for element in &tab.elements {
            let kind = format!("{}{}", "  ".repeat(element.depth()), element.kind.as_str());
            // A table's dimensions belong on its container row: it is the
            // one element kind whose text is empty by construction, so the
            // column would otherwise be dead space on exactly the row a
            // reader most wants to identify.
            let text = match (element.rows, element.columns) {
                (Some(rows), Some(cols)) => format!("{rows}x{cols}"),
                _ => sanitize_for_terminal(&element.text),
            };
            writeln!(
                out,
                "{:>start_w$}  {:>end_w$}  {:<kind_w$}  {:<style_w$}  {}",
                element.start_index,
                element.end_index,
                kind,
                element.style.as_deref().unwrap_or(""),
                text
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
    use crate::drive::docs::read::TabContent;
    use crate::drive::docs::structure::{DocElement, ElementKind};

    fn element(
        start: i64,
        end: i64,
        kind: ElementKind,
        path: Vec<usize>,
        style: Option<&str>,
        text: &str,
    ) -> DocElement {
        DocElement {
            start_index: start,
            end_index: end,
            kind,
            path,
            style: style.map(str::to_string),
            list_id: None,
            rows: None,
            columns: None,
            text: text.to_string(),
        }
    }

    fn outcome(tabs: Vec<TabContent>) -> ReadOutcome {
        ReadOutcome {
            document_id: "d1".to_string(),
            title: Some("Doc".to_string()),
            revision_id: Some("rev-1".to_string()),
            tabs,
        }
    }

    fn tab(id: Option<&str>, title: Option<&str>, elements: Vec<DocElement>) -> TabContent {
        TabContent {
            tab_id: id.map(str::to_string),
            title: title.map(str::to_string),
            nesting_level: 0,
            elements,
        }
    }

    fn render(outcome: &ReadOutcome) -> String {
        let mut buf = Vec::new();
        render_read_table(outcome, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn render_emits_one_line_per_element_with_its_index_range() {
        let text = render(&outcome(vec![tab(
            None,
            None,
            vec![
                element(
                    1,
                    10,
                    ElementKind::Paragraph,
                    vec![0],
                    Some("HEADING_1"),
                    "Overview",
                ),
                element(
                    10,
                    20,
                    ElementKind::Paragraph,
                    vec![1],
                    Some("NORMAL_TEXT"),
                    "Body",
                ),
            ],
        )]));
        assert!(text.contains("START"), "{text}");
        assert!(text.contains("Overview"), "{text}");
        assert!(text.contains("HEADING_1"), "{text}");
        // One header line plus two element lines.
        assert_eq!(text.lines().count(), 3, "{text}");
    }

    #[test]
    fn render_indents_elements_nested_in_a_table() {
        let text = render(&outcome(vec![tab(
            None,
            None,
            vec![
                element(1, 40, ElementKind::Table, vec![0], None, ""),
                element(
                    3,
                    10,
                    ElementKind::Paragraph,
                    vec![0, 0, 0, 0],
                    None,
                    "Name",
                ),
            ],
        )]));
        let nested = text.lines().find(|l| l.contains("Name")).unwrap();
        assert!(nested.contains("      paragraph"), "{nested}");
    }

    #[test]
    fn render_shows_table_dimensions_on_the_container_row() {
        let mut table = element(1, 40, ElementKind::Table, vec![0], None, "");
        table.rows = Some(3);
        table.columns = Some(2);
        let text = render(&outcome(vec![tab(None, None, vec![table])]));
        assert!(text.contains("3x2"), "{text}");
    }

    /// The documented divergence from `sheets read`: this is an orientation
    /// view whose value is alignment, so control bytes are stripped rather
    /// than passed through.
    #[test]
    fn render_sanitizes_element_text() {
        let text = render(&outcome(vec![tab(
            None,
            None,
            vec![element(
                1,
                10,
                ElementKind::Paragraph,
                vec![0],
                None,
                "before\u{1b}[31mred\nnext",
            )],
        )]));
        assert!(!text.contains('\u{1b}'), "{text}");
        // One header line plus one element line — the embedded newline must
        // not have spawned a second row.
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn multiple_tabs_get_a_commented_header_and_a_blank_separator() {
        let text = render(&outcome(vec![
            tab(
                Some("t.0"),
                Some("One"),
                vec![element(1, 5, ElementKind::Paragraph, vec![0], None, "a")],
            ),
            tab(
                Some("t.1"),
                Some("Two"),
                vec![element(1, 5, ElementKind::Paragraph, vec![0], None, "b")],
            ),
        ]));
        assert!(text.contains("# t.0 One"), "{text}");
        assert!(text.contains("# t.1 Two"), "{text}");
        assert!(text.contains("\n\n# t.1"), "{text}");
    }

    #[test]
    fn a_single_tab_renders_without_a_header_line() {
        let text = render(&outcome(vec![tab(
            Some("t.0"),
            Some("One"),
            vec![element(1, 5, ElementKind::Paragraph, vec![0], None, "a")],
        )]));
        assert!(!text.contains("# t.0"), "{text}");
    }

    #[test]
    fn an_empty_tab_says_so_rather_than_printing_a_bare_header() {
        let text = render(&outcome(vec![tab(None, None, vec![])]));
        assert!(text.contains("(empty)"), "{text}");
    }

    #[test]
    fn suggestions_view_arg_maps_onto_the_engine_option() {
        for (arg, mode) in [
            (
                SuggestionsViewArg::Default,
                SuggestionsViewMode::DefaultForCurrentAccess,
            ),
            (SuggestionsViewArg::Inline, SuggestionsViewMode::Inline),
            (
                SuggestionsViewArg::Accepted,
                SuggestionsViewMode::PreviewAccepted,
            ),
            (
                SuggestionsViewArg::Without,
                SuggestionsViewMode::PreviewWithoutSuggestions,
            ),
        ] {
            assert_eq!(SuggestionsViewMode::from(arg), mode, "{arg:?}");
        }
    }
}
