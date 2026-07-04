//! CLI command for diffing two Confluence page versions.
//!
//! Wires up [`crate::atlassian::diff`] (structural ADF diff) and
//! [`crate::atlassian::diff_format`] (output rendering) on top of the
//! Confluence v2 API.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::api::ContentMetadata;
use crate::atlassian::confluence_api::{resolve_version, ConfluenceApi};
use crate::atlassian::confluence_types::PageVersion;
use crate::atlassian::diff::{diff_documents, DiffOptions};
use crate::atlassian::diff_format::{
    render, render_section, CompareContext, CompareOutput, Cursor, Detail, Filter, Includes,
    SectionFormat, VersionInfo, DEFAULT_OUTPUT_BUDGET,
};
use crate::cli::atlassian::format::{output_as, JsonlSerialize, OutputFormat};
use crate::cli::atlassian::helpers::create_client;
use crate::data::yaml::to_yaml;

/// Compares two versions of a Confluence page.
#[derive(Parser)]
pub struct CompareCommand {
    /// Confluence page ID.
    pub id: String,

    /// `from` version reference. Accepts `latest`, `previous`, `v-N`,
    /// a numeric version, or an ISO 8601 date.
    #[arg(long, default_value = "previous")]
    pub from: String,

    /// `to` version reference. Same accepted forms as `--from`.
    #[arg(long, default_value = "latest")]
    pub to: String,

    /// Detail level: `summary`, `outline`, or `full`.
    #[arg(long, value_enum, default_value_t = DetailArg::Outline)]
    pub detail: DetailArg,

    /// Top-level fields to include. Comma-separated. Accepted values:
    /// `body`, `title`, `labels`, `metadata`. Defaults to `body,title,metadata`.
    #[arg(long, default_value = "body,title,metadata")]
    pub include: String,

    /// When set, runs of whitespace inside text nodes are collapsed to a
    /// single space before diffing.
    #[arg(long, default_value_t = true)]
    pub ignore_whitespace: bool,

    /// Drop section deltas with fewer than this many characters of total
    /// changed text. `0` disables the filter.
    #[arg(long, default_value_t = 0)]
    pub min_change_chars: u32,

    /// Restrict to sections whose path matches one of the given strings.
    /// Repeatable: `--filter-section /h2#a --filter-section /h2#b`.
    #[arg(long = "filter-section")]
    pub filter_sections: Vec<String>,

    /// Output budget in bytes. Defaults to ~16 KiB (≈4000 tokens).
    #[arg(long, default_value_t = DEFAULT_OUTPUT_BUDGET)]
    pub budget: usize,

    /// Output format. `yaml` is the most useful target for AI agents.
    //
    // Intentionally defaults to `Yaml` rather than the `Table` used by sibling
    // `-o/--output` commands: a diff summary is consumed by agents, not read as
    // a table. This deviation was reviewed and kept under #1125.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Yaml)]
    pub output: OutputFormat,
}

/// Detail level (CLI surface for [`Detail`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum DetailArg {
    /// Counts only.
    Summary,
    /// Per-section change kind, one-line summaries, drill-in cursors.
    Outline,
    /// Embed per-section deltas. Budget-truncated.
    Full,
}

impl From<DetailArg> for Detail {
    fn from(d: DetailArg) -> Self {
        match d {
            DetailArg::Summary => Self::Summary,
            DetailArg::Outline => Self::Outline,
            DetailArg::Full => Self::Full,
        }
    }
}

/// Drill-in subcommand: returns a per-section diff in a chosen text format.
#[derive(Parser)]
pub struct CompareSectionCommand {
    /// Cursor returned by an outline-mode `confluence compare` call.
    #[arg(long)]
    pub cursor: String,

    /// Output text format.
    #[arg(long, value_enum, default_value_t = SectionFormatArg::Unified)]
    pub format: SectionFormatArg,
}

/// Output format for a single section diff (CLI surface for [`SectionFormat`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SectionFormatArg {
    /// Unified diff (`+`/`-` markers).
    Unified,
    /// Side-by-side: `from` on the left, `to` on the right.
    SideBySide,
    /// Markdown with inline `+added+` / `~~removed~~` markers.
    MarkdownInline,
}

impl From<SectionFormatArg> for SectionFormat {
    fn from(f: SectionFormatArg) -> Self {
        match f {
            SectionFormatArg::Unified => Self::Unified,
            SectionFormatArg::SideBySide => Self::SideBySide,
            SectionFormatArg::MarkdownInline => Self::MarkdownInline,
        }
    }
}

/// Top-level compare command grouping (`compare` + `compare-section`).
#[derive(Parser)]
pub struct CompareCommandGroup {
    /// Subcommand to dispatch.
    #[command(subcommand)]
    pub command: CompareSubcommands,
}

/// Subcommands inside the compare group.
#[derive(Subcommand)]
pub enum CompareSubcommands {
    /// Diff two versions of a Confluence page (mirrors the `confluence_compare` MCP tool).
    Run(CompareCommand),
    /// Drill in to a section diff using a cursor from a prior `run` (mirrors the `confluence_compare_section` MCP tool).
    Section(CompareSectionCommand),
}

impl CompareCommandGroup {
    /// Dispatches to the requested subcommand.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CompareSubcommands::Run(cmd) => cmd.execute().await,
            CompareSubcommands::Section(cmd) => cmd.execute().await,
        }
    }
}

impl CompareCommand {
    /// Executes the command end-to-end (network + render + output).
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        self.run(&api, &instance_url).await
    }

    async fn run(self, api: &ConfluenceApi, instance_url: &str) -> Result<()> {
        let output = self.output.clone();
        let compare = run_compare(api, instance_url, &self).await?;
        if output_as(&compare, &output)? {
            return Ok(());
        }
        // Default (non-`-o` form) prints YAML.
        let yaml = to_yaml(&compare)?;
        println!("{yaml}");
        Ok(())
    }
}

impl JsonlSerialize for CompareOutput {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        crate::cli::atlassian::format::write_scalar_jsonl(self, out)
    }
}

/// Builds a [`CompareOutput`] for the given page and version pair. Used by
/// both the CLI and the MCP tool so the schema stays in lockstep.
pub async fn run_compare(
    api: &ConfluenceApi,
    instance_url: &str,
    cmd: &CompareCommand,
) -> Result<CompareOutput> {
    // 1. List versions (single fetch — feeds both `from` and `to` resolvers).
    //    Cap at 200 versions; pages with deeper history must use explicit
    //    numeric refs.
    let (versions, _truncated) = api
        .list_page_versions(&cmd.id, None, 200)
        .await
        .context("Failed to list page versions")?;
    if versions.is_empty() {
        anyhow::bail!("Page {} has no version history", cmd.id);
    }

    // 2. Resolve `to` first (with anchor = latest), then `from` relative to `to`.
    let latest = versions[0].number;
    let to_v = resolve_version(&cmd.to, &versions, latest)
        .with_context(|| format!("Failed to resolve --to \"{}\"", cmd.to))?;
    let from_v = resolve_version(&cmd.from, &versions, to_v)
        .with_context(|| format!("Failed to resolve --from \"{}\"", cmd.from))?;
    if from_v == to_v {
        anyhow::bail!(
            "--from and --to resolved to the same version ({from_v}); nothing to compare"
        );
    }

    // 3. Fetch each version. Both use `body-format=atlas_doc_format`.
    let (from_item, to_item) = tokio::try_join!(
        api.get_page_at_version(&cmd.id, from_v),
        api.get_page_at_version(&cmd.id, to_v),
    )?;

    // 4. Convert ADF JSON values to AdfDocument trees.
    let from_doc = adf_from_item_body(&from_item, "from")?;
    let to_doc = adf_from_item_body(&to_item, "to")?;

    // 5. Run structural diff.
    let opts = DiffOptions {
        ignore_whitespace: cmd.ignore_whitespace,
    };
    let diff = diff_documents(&from_doc, &to_doc, &opts);

    // 6. Build the render context.
    let from_v_meta = version_info_for(&versions, from_v);
    let to_v_meta = version_info_for(&versions, to_v);
    let url = page_url(instance_url, &to_item);

    let ctx = CompareContext {
        page_id: cmd.id.clone(),
        page_title: to_item.title.clone(),
        page_url: url,
        from_version: from_v_meta,
        to_version: to_v_meta,
        from_title: from_item.title,
        to_title: to_item.title,
        from_labels: Vec::new(),
        to_labels: Vec::new(),
    };

    // 7. Build filter and renderer arguments.
    let includes = parse_includes(&cmd.include)?;
    let filter = Filter {
        sections: cmd.filter_sections.clone(),
        min_change_chars: cmd.min_change_chars,
        kinds: Vec::new(),
    };

    render(diff, &ctx, cmd.detail.into(), includes, &filter, cmd.budget)
}

fn version_info_for(versions: &[PageVersion], n: u32) -> VersionInfo {
    versions
        .iter()
        .find(|v| v.number == n)
        .map(|v| VersionInfo {
            number: v.number,
            created_at: v.created_at.clone(),
            author: v.author_id.clone(),
            message: v.message.clone(),
        })
        .unwrap_or(VersionInfo {
            number: n,
            ..VersionInfo::default()
        })
}

fn adf_from_item_body(
    item: &crate::atlassian::api::ContentItem,
    side: &str,
) -> Result<AdfDocument> {
    match &item.body_adf {
        Some(value) => serde_json::from_value(value.clone())
            .with_context(|| format!("Failed to parse ADF document for {side} version")),
        None => Ok(AdfDocument::default()),
    }
}

fn page_url(instance_url: &str, item: &crate::atlassian::api::ContentItem) -> Option<String> {
    if let ContentMetadata::Confluence { space_key, .. } = &item.metadata {
        if !space_key.is_empty() {
            return Some(format!(
                "{instance_url}/wiki/spaces/{space_key}/pages/{}",
                item.id
            ));
        }
    }
    None
}

fn parse_includes(spec: &str) -> Result<Includes> {
    let mut inc = Includes {
        body: false,
        title: false,
        labels: false,
        metadata: false,
    };
    for raw in spec.split(',') {
        match raw.trim().to_ascii_lowercase().as_str() {
            "body" => inc.body = true,
            "title" => inc.title = true,
            "labels" => inc.labels = true,
            "metadata" => inc.metadata = true,
            "" => {}
            other => return Err(anyhow!("Unknown include flag \"{other}\"")),
        }
    }
    Ok(inc)
}

impl CompareSectionCommand {
    /// Executes the section drill-in.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let cur = Cursor::decode(&self.cursor).context("Invalid --cursor")?;
        let text = run_compare_section(&api, &cur, self.format.into()).await?;
        println!("{text}");
        Ok(())
    }
}

/// Re-fetches the cursor's version pair and renders a single section's
/// diff in the requested format.
pub async fn run_compare_section(
    api: &ConfluenceApi,
    cur: &Cursor,
    format: SectionFormat,
) -> Result<String> {
    let (from_item, to_item) = tokio::try_join!(
        api.get_page_at_version(&cur.page_id, cur.from_v),
        api.get_page_at_version(&cur.page_id, cur.to_v),
    )?;
    let from_doc = adf_from_item_body(&from_item, "from")?;
    let to_doc = adf_from_item_body(&to_item, "to")?;
    let opts = DiffOptions {
        ignore_whitespace: true,
    };
    let diff = diff_documents(&from_doc, &to_doc, &opts);
    render_section(&diff, cur, format)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::atlassian::client::AtlassianClient;
    use serde_json::json;

    async fn setup_api() -> (wiremock::MockServer, ConfluenceApi) {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        (server, api)
    }

    fn page_response(id: &str, title: &str, version: u32, body_text: &str) -> serde_json::Value {
        let adf = format!(
            r#"{{"version":1,"type":"doc","content":[{{"type":"heading","attrs":{{"level":2}},"content":[{{"type":"text","text":"Background"}}]}},{{"type":"paragraph","content":[{{"type":"text","text":"{body_text}"}}]}}]}}"#
        );
        json!({
            "id": id,
            "title": title,
            "status": "current",
            "spaceId": "98",
            "version": {"number": version},
            "body": {
                "atlas_doc_format": {"value": adf}
            }
        })
    }

    async fn mount_page(
        server: &wiremock::MockServer,
        id: &str,
        version: u32,
        title: &str,
        body: &str,
    ) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .and(wiremock::matchers::query_param(
                "version",
                version.to_string(),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_response(id, title, version, body)),
            )
            .mount(server)
            .await;
    }

    async fn mount_versions(server: &wiremock::MockServer, id: &str, results: serde_json::Value) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/pages/{id}/versions"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(json!({ "results": results })),
            )
            .mount(server)
            .await;
    }

    async fn mount_space(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"key": "ENG"})))
            .mount(server)
            .await;
    }

    fn cmd(id: &str) -> CompareCommand {
        CompareCommand {
            id: id.to_string(),
            from: "previous".to_string(),
            to: "latest".to_string(),
            detail: DetailArg::Outline,
            include: "body,title,metadata".to_string(),
            ignore_whitespace: true,
            min_change_chars: 0,
            filter_sections: Vec::new(),
            budget: DEFAULT_OUTPUT_BUDGET,
            output: OutputFormat::Yaml,
        }
    }

    #[test]
    fn detail_arg_to_detail() {
        assert_eq!(Detail::from(DetailArg::Summary), Detail::Summary);
        assert_eq!(Detail::from(DetailArg::Outline), Detail::Outline);
        assert_eq!(Detail::from(DetailArg::Full), Detail::Full);
    }

    #[test]
    fn parse_includes_default_set() {
        let inc = parse_includes("body,title,metadata").unwrap();
        assert!(inc.body && inc.title && inc.metadata);
        assert!(!inc.labels);
    }

    #[test]
    fn parse_includes_with_labels() {
        let inc = parse_includes("body,labels").unwrap();
        assert!(inc.body && inc.labels);
        assert!(!inc.title && !inc.metadata);
    }

    #[test]
    fn parse_includes_unknown_value_errors() {
        let err = parse_includes("body,attachments").unwrap_err();
        assert!(err.to_string().contains("Unknown include flag"));
    }

    #[test]
    fn parse_includes_handles_whitespace_and_empty_segments() {
        let inc = parse_includes("body, , title").unwrap();
        assert!(inc.body && inc.title);
    }

    #[tokio::test]
    async fn run_compare_outline_against_mock() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "alice", "message": "v2", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "bob", "message": "v1", "minorEdit": false},
            ]),
        )
        .await;
        mount_page(&server, "12", 1, "Page v1", "version 12").await;
        mount_page(&server, "12", 2, "Page v2", "version 14").await;
        mount_space(&server).await;

        let out = run_compare(&api, &server.uri(), &cmd("12")).await.unwrap();
        assert_eq!(out.page.id, "12");
        assert_eq!(out.page.title, "Page v2");
        assert_eq!(
            out.versions.as_ref().expect("versions present").from.number,
            1
        );
        assert_eq!(
            out.versions.as_ref().expect("versions present").to.number,
            2
        );
        // Background section was modified (12 → 14).
        assert!(out.summary.by_kind.sections_modified >= 1);
        let bg = out
            .sections
            .iter()
            .find(|s| s.path == "/h2#background")
            .expect("background section");
        assert!(!bg.cursor.is_empty());
    }

    #[tokio::test]
    async fn run_compare_same_version_errors() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 5, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        let mut c = cmd("12");
        c.from = "5".to_string();
        c.to = "latest".to_string();
        let err = run_compare(&api, &server.uri(), &c).await.unwrap_err();
        assert!(err.to_string().contains("same version"));
    }

    #[tokio::test]
    async fn run_compare_no_versions_errors() {
        let (server, api) = setup_api().await;
        mount_versions(&server, "12", json!([])).await;
        let err = run_compare(&api, &server.uri(), &cmd("12"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no version history"));
    }

    #[tokio::test]
    async fn run_compare_section_round_trip() {
        let (server, api) = setup_api().await;
        mount_page(&server, "12", 1, "T", "version 12").await;
        mount_page(&server, "12", 2, "T", "version 14").await;
        mount_space(&server).await;

        let cur = Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        };
        let text = run_compare_section(&api, &cur, SectionFormat::Unified)
            .await
            .unwrap();
        assert!(text.contains("/h2#background"));
        assert!(text.contains("version 12"));
        assert!(text.contains("version 14"));
    }

    #[tokio::test]
    async fn run_compare_full_includes_diff_payload() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        mount_page(&server, "12", 1, "T", "version 12").await;
        mount_page(&server, "12", 2, "T", "version 14").await;
        mount_space(&server).await;

        let mut c = cmd("12");
        c.detail = DetailArg::Full;
        let out = run_compare(&api, &server.uri(), &c).await.unwrap();
        let bg = out
            .sections
            .iter()
            .find(|s| s.path == "/h2#background")
            .expect("background section");
        assert!(!bg.diff.is_empty(), "full mode should embed diff payload");
    }

    #[tokio::test]
    async fn run_compare_summary_omits_sections() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        mount_page(&server, "12", 1, "T", "v1").await;
        mount_page(&server, "12", 2, "T", "v2").await;
        mount_space(&server).await;

        let mut c = cmd("12");
        c.detail = DetailArg::Summary;
        let out = run_compare(&api, &server.uri(), &c).await.unwrap();
        assert!(out.sections.is_empty());
        assert!(out.summary.total_changes >= 1);
    }

    // ── SectionFormatArg::From ────────────────────────────────────

    #[test]
    fn section_format_arg_to_section_format() {
        assert_eq!(
            SectionFormat::from(SectionFormatArg::Unified),
            SectionFormat::Unified
        );
        assert_eq!(
            SectionFormat::from(SectionFormatArg::SideBySide),
            SectionFormat::SideBySide
        );
        assert_eq!(
            SectionFormat::from(SectionFormatArg::MarkdownInline),
            SectionFormat::MarkdownInline
        );
    }

    // ── parse_includes: empty / unknown ───────────────────────────

    #[test]
    fn parse_includes_empty_returns_all_disabled() {
        let inc = parse_includes("").unwrap();
        assert!(!inc.body && !inc.title && !inc.labels && !inc.metadata);
    }

    #[test]
    fn parse_includes_all_four_flags() {
        let inc = parse_includes("body,title,labels,metadata").unwrap();
        assert!(inc.body && inc.title && inc.labels && inc.metadata);
    }

    // ── version_info_for: hit and miss ────────────────────────────

    #[test]
    fn version_info_for_hit_returns_full_metadata() {
        let versions = vec![PageVersion {
            number: 4,
            created_at: "2026-05-08T10:00:00Z".to_string(),
            author_id: "alice".to_string(),
            message: "v4".to_string(),
            minor_edit: false,
        }];
        let info = version_info_for(&versions, 4);
        assert_eq!(info.number, 4);
        assert_eq!(info.author, "alice");
        assert_eq!(info.message, "v4");
    }

    #[test]
    fn version_info_for_miss_returns_default_with_number() {
        let info = version_info_for(&[], 99);
        assert_eq!(info.number, 99);
        assert!(info.created_at.is_empty());
        assert!(info.author.is_empty());
    }

    // ── page_url ──────────────────────────────────────────────────

    #[test]
    fn page_url_built_when_space_key_present() {
        use crate::atlassian::api::{ContentItem, ContentMetadata};
        let item = ContentItem {
            id: "12".to_string(),
            title: "T".to_string(),
            body_adf: None,
            metadata: ContentMetadata::Confluence {
                space_key: "ENG".to_string(),
                status: None,
                version: Some(1),
                parent_id: None,
            },
        };
        assert_eq!(
            page_url("https://x.atlassian.net", &item).as_deref(),
            Some("https://x.atlassian.net/wiki/spaces/ENG/pages/12")
        );
    }

    #[test]
    fn page_url_none_when_space_key_empty() {
        use crate::atlassian::api::{ContentItem, ContentMetadata};
        let item = ContentItem {
            id: "12".to_string(),
            title: "T".to_string(),
            body_adf: None,
            metadata: ContentMetadata::Confluence {
                space_key: String::new(),
                status: None,
                version: None,
                parent_id: None,
            },
        };
        assert!(page_url("https://x.atlassian.net", &item).is_none());
    }

    #[test]
    fn page_url_none_when_jira_metadata() {
        use crate::atlassian::api::{ContentItem, ContentMetadata};
        let item = ContentItem {
            id: "PROJ-1".to_string(),
            title: "T".to_string(),
            body_adf: None,
            metadata: ContentMetadata::Jira {
                status: Some("Open".to_string()),
                issue_type: Some("Bug".to_string()),
                assignee: None,
                priority: None,
                labels: Vec::new(),
            },
        };
        assert!(page_url("https://x.atlassian.net", &item).is_none());
    }

    // ── run_compare error: failed list_versions ───────────────────

    #[tokio::test]
    async fn run_compare_propagates_list_versions_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let err = run_compare(&api, &server.uri(), &cmd("12"))
            .await
            .unwrap_err();
        // The error from list_page_versions is wrapped in a context.
        assert!(
            err.to_string().contains("Failed to list page versions"),
            "got: {err}"
        );
    }

    // ── run_compare error: bad include ────────────────────────────

    #[tokio::test]
    async fn run_compare_propagates_bad_include() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        mount_page(&server, "12", 1, "T", "v1").await;
        mount_page(&server, "12", 2, "T", "v2").await;
        mount_space(&server).await;
        let mut c = cmd("12");
        c.include = "body,bogus".to_string();
        let err = run_compare(&api, &server.uri(), &c).await.unwrap_err();
        assert!(err.to_string().contains("Unknown include flag"));
    }

    // ── run_compare error: bad to ─────────────────────────────────

    #[tokio::test]
    async fn run_compare_propagates_bad_to() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        let mut c = cmd("12");
        c.to = "garbage".to_string();
        let err = run_compare(&api, &server.uri(), &c).await.unwrap_err();
        assert!(err.to_string().contains("Failed to resolve --to"));
    }

    // ── run_compare error: bad from ───────────────────────────────

    #[tokio::test]
    async fn run_compare_propagates_bad_from() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        let mut c = cmd("12");
        c.from = "garbage".to_string();
        let err = run_compare(&api, &server.uri(), &c).await.unwrap_err();
        assert!(err.to_string().contains("Failed to resolve --from"));
    }

    // ── adf_from_item_body: missing body returns default ─────────

    #[test]
    fn adf_from_item_body_missing_body_returns_default() {
        use crate::atlassian::api::{ContentItem, ContentMetadata};
        let item = ContentItem {
            id: "12".to_string(),
            title: "T".to_string(),
            body_adf: None,
            metadata: ContentMetadata::Confluence {
                space_key: "ENG".to_string(),
                status: None,
                version: None,
                parent_id: None,
            },
        };
        let doc = adf_from_item_body(&item, "test").unwrap();
        assert_eq!(doc.content.len(), 0);
    }

    #[test]
    fn adf_from_item_body_invalid_json_errors() {
        use crate::atlassian::api::{ContentItem, ContentMetadata};
        let item = ContentItem {
            id: "12".to_string(),
            title: "T".to_string(),
            // Wrong shape — missing required fields.
            body_adf: Some(json!({"unexpected": "shape"})),
            metadata: ContentMetadata::Confluence {
                space_key: "ENG".to_string(),
                status: None,
                version: None,
                parent_id: None,
            },
        };
        let err = adf_from_item_body(&item, "from").unwrap_err();
        assert!(err.to_string().contains("from"));
    }

    // ── run_compare_section returns rendered text ─────────────────

    #[tokio::test]
    async fn run_compare_section_unified_via_helper() {
        let (server, api) = setup_api().await;
        mount_page(&server, "12", 1, "T", "v1").await;
        mount_page(&server, "12", 2, "T", "v2").await;
        mount_space(&server).await;
        let cur = Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        };
        let text = run_compare_section(&api, &cur, SectionFormat::Unified)
            .await
            .unwrap();
        assert!(text.contains("/h2#background"));
    }

    // ── execute paths via env-based create_client ─────────────────

    /// Sets Atlassian credentials, runs a closure that may call
    /// `create_client()`, then unsets credentials. Tests serialize on the one
    /// canonical env mutex (issue #950) because env vars are process-global —
    /// an independent lock would not exclude the other Atlassian credential
    /// tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compare_command_execute_dispatches_with_creds() {
        let _lock = env_lock();
        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "u@t.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake");

        let cmd = CompareCommand {
            id: "12".to_string(),
            from: "previous".to_string(),
            to: "latest".to_string(),
            detail: DetailArg::Outline,
            include: "body,title,metadata".to_string(),
            ignore_whitespace: true,
            min_change_chars: 0,
            filter_sections: Vec::new(),
            budget: DEFAULT_OUTPUT_BUDGET,
            output: OutputFormat::Yaml,
        };
        // Allow this to fail (no real server); we only care that the dispatch
        // line runs and exercise create_client + the run path.
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compare_section_command_execute_dispatches_with_creds() {
        let _lock = env_lock();
        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "u@t.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake");

        let cur = Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        };
        let cmd = CompareSectionCommand {
            cursor: cur.encode().unwrap(),
            format: SectionFormatArg::Unified,
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compare_section_command_execute_happy_path_via_mock() {
        // Pointing ATLASSIAN_INSTANCE_URL at a real (mock) server lets the
        // happy-path body of `CompareSectionCommand::execute` run all the way
        // through `println!` + `Ok(())` — exercises the success branch that
        // the dispatch-only test cannot reach.
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        // Both versions for the cursor.
        let mount_page = |version: u32, body: &'static str| {
            let s = &server;
            async move {
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/wiki/api/v2/pages/12"))
                    .and(wiremock::matchers::query_param(
                        "version",
                        version.to_string(),
                    ))
                    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                        "id": "12",
                        "title": "T",
                        "status": "current",
                        "spaceId": "98",
                        "version": {"number": version},
                        "body": {"atlas_doc_format": {"value": body}}
                    })))
                    .mount(s)
                    .await;
            }
        };
        mount_page(
            1,
            r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Background"}]},{"type":"paragraph","content":[{"type":"text","text":"v1"}]}]}"#,
        )
        .await;
        mount_page(
            2,
            r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Background"}]},{"type":"paragraph","content":[{"type":"text","text":"v2"}]}]}"#,
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"key": "ENG"})))
            .mount(&server)
            .await;

        std::env::set_var("ATLASSIAN_INSTANCE_URL", server.uri());
        std::env::set_var("ATLASSIAN_EMAIL", "u@t.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake");

        let cur = Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        };
        let cmd = CompareSectionCommand {
            cursor: cur.encode().unwrap(),
            format: SectionFormatArg::Unified,
        };
        cmd.execute().await.unwrap();

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compare_command_execute_happy_path_via_mock() {
        // Drives `CompareCommand::execute` all the way through to the
        // YAML-print branch using a real wiremock server.
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                    {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
                ]
            })))
            .mount(&server)
            .await;
        let mount_page = |version: u32, body: &'static str| {
            let s = &server;
            async move {
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/wiki/api/v2/pages/12"))
                    .and(wiremock::matchers::query_param(
                        "version",
                        version.to_string(),
                    ))
                    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                        "id": "12",
                        "title": "T",
                        "status": "current",
                        "spaceId": "98",
                        "version": {"number": version},
                        "body": {"atlas_doc_format": {"value": body}}
                    })))
                    .mount(s)
                    .await;
            }
        };
        mount_page(
            1,
            r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"H"}]},{"type":"paragraph","content":[{"type":"text","text":"v1"}]}]}"#,
        )
        .await;
        mount_page(
            2,
            r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"H"}]},{"type":"paragraph","content":[{"type":"text","text":"v2"}]}]}"#,
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"key": "ENG"})))
            .mount(&server)
            .await;

        std::env::set_var("ATLASSIAN_INSTANCE_URL", server.uri());
        std::env::set_var("ATLASSIAN_EMAIL", "u@t.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake");

        let cmd = CompareCommand {
            id: "12".to_string(),
            from: "previous".to_string(),
            to: "latest".to_string(),
            detail: DetailArg::Outline,
            include: "body,title,metadata".to_string(),
            ignore_whitespace: true,
            min_change_chars: 0,
            filter_sections: Vec::new(),
            budget: DEFAULT_OUTPUT_BUDGET,
            output: OutputFormat::Yaml,
        };
        cmd.execute().await.unwrap();

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compare_section_command_execute_invalid_cursor_errors() {
        let _lock = env_lock();
        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "u@t.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake");

        let cmd = CompareSectionCommand {
            cursor: "!!!not-a-cursor".to_string(),
            format: SectionFormatArg::Unified,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("Invalid --cursor"));

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compare_command_group_execute_dispatches_run() {
        let _lock = env_lock();
        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "u@t.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake");

        let group = CompareCommandGroup {
            command: CompareSubcommands::Run(CompareCommand {
                id: "12".to_string(),
                from: "previous".to_string(),
                to: "latest".to_string(),
                detail: DetailArg::Outline,
                include: "body".to_string(),
                ignore_whitespace: true,
                min_change_chars: 0,
                filter_sections: Vec::new(),
                budget: DEFAULT_OUTPUT_BUDGET,
                output: OutputFormat::Yaml,
            }),
        };
        let _ = group.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compare_command_group_execute_dispatches_section() {
        let _lock = env_lock();
        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "u@t.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake");

        let cur = Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        };
        let group = CompareCommandGroup {
            command: CompareSubcommands::Section(CompareSectionCommand {
                cursor: cur.encode().unwrap(),
                format: SectionFormatArg::Unified,
            }),
        };
        let _ = group.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    // ── CompareCommand::run prints YAML on default output ─────────

    #[tokio::test]
    async fn compare_command_run_prints_yaml_for_default_output() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        mount_page(&server, "12", 1, "T", "v1").await;
        mount_page(&server, "12", 2, "T", "v2").await;
        mount_space(&server).await;

        let mut c = cmd("12");
        // Force the YAML default-print path (not handled by output_as).
        c.output = OutputFormat::Table;
        c.run(&api, &server.uri()).await.unwrap();
    }

    #[tokio::test]
    async fn compare_command_run_yaml_explicit_output() {
        let (server, api) = setup_api().await;
        mount_versions(
            &server,
            "12",
            json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        mount_page(&server, "12", 1, "T", "v1").await;
        mount_page(&server, "12", 2, "T", "v2").await;
        mount_space(&server).await;

        let c = cmd("12");
        c.run(&api, &server.uri()).await.unwrap();
    }

    // ── JsonlSerialize for CompareOutput ──────────────────────────

    #[test]
    fn compare_output_jsonl_emits_single_line() {
        use crate::atlassian::diff_format::{
            ByKind, CompareOutput, NetCounts, PageHeader, SummaryBlock,
        };
        let out = CompareOutput {
            page: PageHeader {
                id: "1".to_string(),
                title: "T".to_string(),
                url: None,
            },
            versions: None,
            summary: SummaryBlock {
                total_changes: 0,
                by_kind: ByKind::default(),
                net: NetCounts::default(),
            },
            title_change: None,
            labels: None,
            sections: Vec::new(),
            truncated: false,
            continuation: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        out.write_jsonl(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.lines().count(), 1);
        assert!(s.contains("\"id\":\"1\""));
    }
}
