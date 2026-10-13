//! Drift detection between the gateway's field policy and the current
//! Anthropic Messages API.
//!
//! Anthropic publishes no machine-readable schema URL any more (the SDKs'
//! `.stats.yml` stopped naming the OpenAPI spec), but the official Python SDK's
//! types are generated from it and are plain text. A weekly job
//! (`.github/workflows/anthropic-api-drift.yml`, via `bin/anthropic-api-drift`)
//! fetches them and compares:
//!
//! - top-level request fields (GA and beta) with [`REQUEST_FIELDS`];
//! - message content-block types with [`CONTENT_BLOCK_TYPES`];
//! - client-tool fields with [`TOOL_FIELDS`], and Anthropic-defined tool
//!   types with [`ANTHROPIC_TOOL_TYPE_PREFIXES`];
//! - thinking types and display modes, stop reasons, and stream delta types.
//!
//! Anything upstream has that the tables do not is drift: it would hit the
//! gateway's unknown-field rejection, so it needs classing. Entries only the
//! gateway knows (legacy fields such as `temperature`, or Claude Code
//! extensions such as `safeguards`) are listed for information. The parser
//! only understands the patterns the SDK uses; a layout change surfaces as an
//! error, never as a silent empty diff.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Serialize;

use super::policy::{
    FieldRule, ANTHROPIC_TOOL_TYPE_PREFIXES, CONTENT_BLOCK_TYPES, REQUEST_FIELDS, STOP_REASONS,
    STREAM_DELTA_TYPES, THINKING_DISPLAYS, THINKING_TYPES, TOOL_FIELDS,
};

/// Default location of the SDK's generated types.
pub const DEFAULT_TYPES_URL: &str =
    "https://raw.githubusercontent.com/anthropics/anthropic-sdk-python/main/src/anthropic/types";

/// Environment override for [`DEFAULT_TYPES_URL`] (tests, mirrors).
pub const TYPES_URL_ENV: &str = "OMNI_DEV_ANTHROPIC_SDK_TYPES_URL";

/// Request parameters that are not body fields (sent as headers).
const NON_BODY_PARAMS: &[&str] = &["betas"];

/// One compared category.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CategoryDrift {
    /// Upstream has these; the gateway's table does not.
    pub new_upstream: BTreeSet<String>,
    /// The gateway's table has these; upstream does not (informational).
    pub local_only: BTreeSet<String>,
}

impl CategoryDrift {
    fn compare(upstream: &BTreeSet<String>, local: &BTreeSet<String>) -> Self {
        Self {
            new_upstream: upstream.difference(local).cloned().collect(),
            local_only: local.difference(upstream).cloned().collect(),
        }
    }
}

/// The comparison of every category.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DriftReport {
    /// By category name.
    pub categories: BTreeMap<String, CategoryDrift>,
}

impl DriftReport {
    /// Whether any category has something upstream the gateway does not know.
    #[must_use]
    pub fn has_drift(&self) -> bool {
        self.categories.values().any(|c| !c.new_upstream.is_empty())
    }

    /// The report as Markdown, for a tracking issue.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = String::from("## Anthropic API drift\n\n");
        if self.has_drift() {
            out.push_str(
                "The Anthropic SDK's generated types name things the gateway's field policy \
                 (`src/gateway/policy.rs`) does not class. Until they are classed, requests \
                 using them are rejected as unknown. Class each as translated, ignored or \
                 rejected, and update `docs/anthropic-gateway.md`.\n\n",
            );
        } else {
            out.push_str("No drift: the gateway's policy classes everything upstream names.\n\n");
        }
        for (name, category) in &self.categories {
            let _ = writeln!(out, "### {name}\n");
            let list = |set: &BTreeSet<String>| {
                if set.is_empty() {
                    "none".to_string()
                } else {
                    set.iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            };
            let _ = writeln!(out, "- **New upstream:** {}", list(&category.new_upstream));
            let _ = writeln!(out, "- Gateway only: {}\n", list(&category.local_only));
        }
        out
    }

    /// Writes `drift-report.md` and `drift-report.json` to `dir`, appends
    /// `drift=<bool>` to the GitHub Actions output file when given, and echoes
    /// the signal to `stdout`.
    pub fn write(
        &self,
        dir: &Path,
        github_output: Option<&Path>,
        stdout: &mut dyn std::io::Write,
    ) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join("drift-report.md"), self.render_markdown())?;
        std::fs::write(
            dir.join("drift-report.json"),
            serde_json::to_string_pretty(self)?,
        )?;
        let line = format!("drift={}\n", self.has_drift());
        if let Some(path) = github_output {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            file.write_all(line.as_bytes())?;
        }
        stdout.write_all(line.as_bytes())?;
        Ok(())
    }
}

/// Fetches the SDK types and compares them with the gateway's tables.
pub async fn fetch_drift_report() -> Result<DriftReport> {
    let base = std::env::var(TYPES_URL_ENV).unwrap_or_else(|_| DEFAULT_TYPES_URL.to_string());
    fetch_drift_report_from(&base).await
}

/// [`fetch_drift_report`] from an explicit types URL.
pub async fn fetch_drift_report_from(base: &str) -> Result<DriftReport> {
    let base = base.to_string();
    let client = reqwest::Client::builder()
        .user_agent(concat!("omni-dev/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let source = HttpSource { client, base };
    let upstream = UpstreamSurface::load(&source).await?;
    Ok(upstream.compare())
}

struct HttpSource {
    client: reqwest::Client,
    base: String,
}

impl HttpSource {
    async fn get(&self, module: &str) -> Result<String> {
        let url = format!("{}/{module}.py", self.base.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        if !response.status().is_success() {
            bail!("fetching {url}: HTTP {}", response.status());
        }
        Ok(response.text().await?)
    }
}

/// The parts of the API the gateway classes, as the SDK names them.
#[derive(Debug, Default)]
struct UpstreamSurface {
    request_fields: BTreeSet<String>,
    content_blocks: BTreeSet<String>,
    tool_fields: BTreeSet<String>,
    tool_types: BTreeSet<String>,
    thinking_types: BTreeSet<String>,
    thinking_displays: BTreeSet<String>,
    stop_reasons: BTreeSet<String>,
    stream_deltas: BTreeSet<String>,
}

impl UpstreamSurface {
    async fn load(source: &HttpSource) -> Result<Self> {
        let mut surface = Self::default();
        for module in ["message_create_params", "beta/message_create_params"] {
            let src = source.get(module).await?;
            for class in ["MessageCreateParamsBase", "MessageCreateParamsStreaming"] {
                surface.request_fields.extend(class_fields(&src, class));
            }
        }
        for name in NON_BODY_PARAMS {
            surface.request_fields.remove(*name);
        }
        if surface.request_fields.len() < 5 {
            bail!(
                "parsed only {} request fields; the SDK layout changed",
                surface.request_fields.len()
            );
        }

        surface.content_blocks = union_type_literals(source, "message_param", "BlockParam").await?;
        surface.tool_types = union_type_literals(source, "tool_union_param", "Param").await?;
        surface.thinking_types =
            union_type_literals(source, "thinking_config_param", "Param").await?;
        surface.stream_deltas =
            union_type_literals(source, "raw_content_block_delta", "Delta").await?;
        surface.tool_fields = class_fields(&source.get("tool_param").await?, "ToolParam");
        surface.stop_reasons = quoted(&source.get("stop_reason").await?);
        let adaptive = source.get("thinking_config_adaptive_param").await?;
        surface.thinking_displays = field_literals(&adaptive, "display");
        for (name, set) in [
            ("content blocks", &surface.content_blocks),
            ("tool types", &surface.tool_types),
            ("thinking types", &surface.thinking_types),
            ("stream deltas", &surface.stream_deltas),
            ("tool fields", &surface.tool_fields),
            ("stop reasons", &surface.stop_reasons),
            ("thinking displays", &surface.thinking_displays),
        ] {
            if set.is_empty() {
                bail!("parsed no {name}; the SDK layout changed");
            }
        }
        Ok(surface)
    }

    fn compare(&self) -> DriftReport {
        let names = |table: &[FieldRule]| {
            table
                .iter()
                .map(|r| r.name.to_string())
                .collect::<BTreeSet<_>>()
        };
        let strings = |list: &[&str]| {
            list.iter()
                .map(|s| (*s).to_string())
                .collect::<BTreeSet<_>>()
        };
        let mut categories = BTreeMap::new();
        categories.insert(
            "request fields".to_string(),
            CategoryDrift::compare(&self.request_fields, &names(REQUEST_FIELDS)),
        );
        categories.insert(
            "content blocks".to_string(),
            CategoryDrift::compare(&self.content_blocks, &names(CONTENT_BLOCK_TYPES)),
        );
        categories.insert(
            "tool fields".to_string(),
            CategoryDrift::compare(&self.tool_fields, &names(TOOL_FIELDS)),
        );
        let unclassified: BTreeSet<String> = self
            .tool_types
            .iter()
            .filter(|t| {
                *t != "custom"
                    && !ANTHROPIC_TOOL_TYPE_PREFIXES
                        .iter()
                        .any(|p| t.starts_with(p))
            })
            .cloned()
            .collect();
        categories.insert(
            "tool types (all rejected; listed when no prefix names them)".to_string(),
            CategoryDrift {
                new_upstream: unclassified,
                local_only: BTreeSet::new(),
            },
        );
        categories.insert(
            "thinking types".to_string(),
            CategoryDrift::compare(&self.thinking_types, &strings(THINKING_TYPES)),
        );
        categories.insert(
            "thinking displays".to_string(),
            CategoryDrift::compare(&self.thinking_displays, &strings(THINKING_DISPLAYS)),
        );
        categories.insert(
            "stop reasons".to_string(),
            CategoryDrift::compare(&self.stop_reasons, &names(STOP_REASONS)),
        );
        categories.insert(
            "stream deltas".to_string(),
            CategoryDrift::compare(&self.stream_deltas, &names(STREAM_DELTA_TYPES)),
        );
        DriftReport { categories }
    }
}

/// Removes docstrings, so field-shaped lines inside them are not read.
fn strip_docstrings(src: &str) -> String {
    #[allow(clippy::unwrap_used)] // A literal pattern.
    let re = Regex::new(r#"(?s)""".*?""""#).unwrap();
    re.replace_all(src, "").into_owned()
}

/// The lines declared directly in `class <name>(...)`, docstrings removed.
fn class_body(src: &str, class: &str) -> String {
    let src = strip_docstrings(src);
    let mut body = String::new();
    let mut inside = false;
    for line in src.lines() {
        if line.starts_with("class ") {
            inside = line.starts_with(&format!("class {class}("));
            continue;
        }
        if inside && !line.is_empty() && !line.starts_with(' ') {
            inside = false;
        }
        if inside {
            body.push_str(line);
            body.push('\n');
        }
    }
    body
}

/// The field names declared directly in `class <name>(...)`.
fn class_fields(src: &str, class: &str) -> BTreeSet<String> {
    #[allow(clippy::unwrap_used)] // A literal pattern.
    let field = Regex::new(r"^    ([a-z_][a-z0-9_]*):").unwrap();
    class_body(src, class)
        .lines()
        .filter_map(|line| field.captures(line).map(|c| c[1].to_string()))
        .collect()
}

/// Every double-quoted string in `src` (for `Literal[...]` aliases).
fn quoted(src: &str) -> BTreeSet<String> {
    #[allow(clippy::unwrap_used)] // A literal pattern.
    let re = Regex::new(r#""([a-z_0-9]+)""#).unwrap();
    let src = strip_docstrings(src);
    re.captures_iter(&src).map(|c| c[1].to_string()).collect()
}

/// The `Literal[...]` values of field `name`.
fn field_literals(src: &str, name: &str) -> BTreeSet<String> {
    let src = strip_docstrings(src);
    src.lines()
        .filter(|l| l.trim_start().starts_with(&format!("{name}:")) && l.contains("Literal["))
        .flat_map(quoted)
        .collect()
}

/// `module → [ClassName]` for `from .module import A, B` (also parenthesized).
fn imports(src: &str) -> Vec<(String, Vec<String>)> {
    #[allow(clippy::unwrap_used)] // A literal pattern.
    let re = Regex::new(r"(?m)^from \.([a-z0-9_]+) import (\([^)]*\)|[^\n]+)").unwrap();
    re.captures_iter(src)
        .map(|c| {
            let names = c[2]
                .trim_matches(|ch| ch == '(' || ch == ')')
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect();
            (c[1].to_string(), names)
        })
        .collect()
}

/// The `type` literals of every class `union_module` imports whose name ends
/// with `suffix`.
async fn union_type_literals(
    source: &HttpSource,
    union_module: &str,
    suffix: &str,
) -> Result<BTreeSet<String>> {
    let src = source.get(union_module).await?;
    let mut out = BTreeSet::new();
    for (module, names) in imports(&src) {
        let classes: Vec<&String> = names.iter().filter(|n| n.ends_with(suffix)).collect();
        if classes.is_empty() {
            continue;
        }
        let member = source.get(&module).await?;
        for class in classes {
            out.extend(field_literals(&class_body(&member, class), "type"));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PARAMS: &str = r#"
from .tool_param import ToolParam

class MessageCreateParamsBase(TypedDict, total=False):
    max_tokens: Required[int]
    """Docs.

    Example: not a field
    """

    messages: Required[Iterable[MessageParam]]
    brand_new: str

Metadata: TypeAlias = MetadataParam

class MessageCreateParamsStreaming(MessageCreateParamsBase):
    stream: Required[Literal[True]]
"#;

    #[test]
    fn class_fields_skip_docstrings_and_other_classes() {
        let fields = class_fields(PARAMS, "MessageCreateParamsBase");
        let expected: BTreeSet<String> = ["max_tokens", "messages", "brand_new"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(fields, expected);
        assert_eq!(
            class_fields(PARAMS, "MessageCreateParamsStreaming").len(),
            1
        );
        assert!(class_fields(PARAMS, "Missing").is_empty());
    }

    #[test]
    fn imports_and_literals_parse() {
        let src = "from .a_param import AParam\nfrom .b import (\n    BParam,\n    Other,\n)\n";
        let parsed = imports(src);
        assert_eq!(
            parsed[0],
            ("a_param".to_string(), vec!["AParam".to_string()])
        );
        assert_eq!(parsed[1].1, vec!["BParam".to_string(), "Other".to_string()]);
        let literal = "class X(TypedDict):\n    type: Required[Literal[\"text\"]]\n    display: Optional[Literal[\"a\", \"b\"]]\n";
        assert_eq!(
            field_literals(literal, "type")
                .into_iter()
                .collect::<Vec<_>>(),
            ["text"]
        );
        assert_eq!(field_literals(literal, "display").len(), 2);
        assert_eq!(quoted("X = Literal[\"end_turn\", \"refusal\"]").len(), 2);
    }

    fn report_with(category: &str, new_upstream: &[&str]) -> DriftReport {
        let mut categories = BTreeMap::new();
        categories.insert(
            category.to_string(),
            CategoryDrift {
                new_upstream: new_upstream.iter().map(|s| s.to_string()).collect(),
                local_only: ["safeguards".to_string()].into(),
            },
        );
        DriftReport { categories }
    }

    #[test]
    fn rendering_and_writing() {
        let drift = report_with("request fields", &["brand_new"]);
        assert!(drift.has_drift());
        let md = drift.render_markdown();
        assert!(md.contains("**New upstream:** `brand_new`"));
        assert!(md.contains("Gateway only: `safeguards`"));
        let clean = report_with("request fields", &[]);
        assert!(!clean.has_drift());
        assert!(
            clean.render_markdown().contains("No drift")
                && clean.render_markdown().contains("none")
        );

        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("gh-output");
        let mut stdout = Vec::new();
        drift
            .write(&dir.path().join("out"), Some(&output), &mut stdout)
            .unwrap();
        assert_eq!(String::from_utf8(stdout).unwrap(), "drift=true\n");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "drift=true\n");
        assert!(dir.path().join("out/drift-report.json").exists());
        assert!(dir.path().join("out/drift-report.md").exists());
    }

    /// Serves a miniature SDK tree where everything matches the tables except
    /// one new request field.
    async fn mini_sdk() -> MockServer {
        let server = MockServer::start().await;
        let request_fields: Vec<String> = REQUEST_FIELDS
            .iter()
            .filter(|r| !matches!(r.name, "temperature" | "top_p" | "top_k" | "safeguards"))
            .map(|r| format!("    {}: object", r.name))
            .collect();
        let params = format!(
            "class MessageCreateParamsBase(TypedDict):\n{}\n    future_field: str\n    betas: list\n",
            request_fields.join("\n")
        );
        let union = |members: &[(&str, &str)]| -> String {
            members
                .iter()
                .map(|(module, class)| format!("from .{module} import {class}\n"))
                .collect()
        };
        let typed = |class: &str, value: &str| {
            format!("class {class}(TypedDict):\n    type: Required[Literal[\"{value}\"]]\n")
        };
        let mut files: Vec<(String, String)> = vec![
            ("message_create_params".into(), params.clone()),
            ("beta/message_create_params".into(), params),
            ("stop_reason".into(), format!("StopReason = Literal[{}]", STOP_REASONS.iter().map(|r| format!("\"{}\"", r.name)).collect::<Vec<_>>().join(", "))),
            ("thinking_config_adaptive_param".into(), "class A(TypedDict):\n    display: Optional[Literal[\"summarized\", \"omitted\", \"updates\"]]\n".into()),
            ("tool_param".into(), format!("class ToolParam(TypedDict):\n{}\n", TOOL_FIELDS.iter().map(|r| format!("    {}: object", r.name)).collect::<Vec<_>>().join("\n"))),
        ];
        let blocks: Vec<(String, String)> = CONTENT_BLOCK_TYPES
            .iter()
            .map(|r| {
                (
                    format!("{}_block_param", r.name),
                    format!("{}BlockParam", r.name),
                )
            })
            .collect();
        files.push((
            "message_param".into(),
            union(
                &blocks
                    .iter()
                    .map(|(m, c)| (m.as_str(), c.as_str()))
                    .collect::<Vec<_>>(),
            ),
        ));
        for ((module, class), rule) in blocks.iter().zip(CONTENT_BLOCK_TYPES) {
            files.push((module.clone(), typed(class, rule.name)));
        }
        files.push((
            "tool_union_param".into(),
            union(&[
                ("tool_param", "ToolParam"),
                ("tool_bash_param", "ToolBashParam"),
                ("tool_quantum_param", "ToolQuantumParam"),
            ]),
        ));
        files.push((
            "tool_bash_param".into(),
            typed("ToolBashParam", "bash_20250124"),
        ));
        files.push((
            "tool_quantum_param".into(),
            typed("ToolQuantumParam", "quantum_20270101"),
        ));
        let thinking: Vec<(String, String)> = THINKING_TYPES
            .iter()
            .map(|t| (format!("thinking_config_{t}_param"), format!("T{t}Param")))
            .collect();
        files.push((
            "thinking_config_param".into(),
            union(
                &thinking
                    .iter()
                    .map(|(m, c)| (m.as_str(), c.as_str()))
                    .collect::<Vec<_>>(),
            ),
        ));
        for ((module, class), t) in thinking.iter().zip(THINKING_TYPES) {
            files.push((module.clone(), typed(class, t)));
        }
        let deltas: Vec<(String, String)> = STREAM_DELTA_TYPES
            .iter()
            .map(|r| (r.name.to_string(), format!("{}Delta", r.name)))
            .collect();
        files.push((
            "raw_content_block_delta".into(),
            union(
                &deltas
                    .iter()
                    .map(|(m, c)| (m.as_str(), c.as_str()))
                    .collect::<Vec<_>>(),
            ),
        ));
        for ((module, class), rule) in deltas.iter().zip(STREAM_DELTA_TYPES) {
            files.push((module.clone(), typed(class, rule.name)));
        }
        // The tool_param module doubles as a union member; give it a type literal too.
        files.retain(|(m, _)| m != "tool_param");
        files.push((
            "tool_param".into(),
            format!(
                "class InputSchemaTyped(TypedDict):\n    type: Required[Literal[\"object\"]]\n\n\
                 class ToolParam(TypedDict):\n    type: Optional[Literal[\"custom\"]]\n{}\n",
                TOOL_FIELDS
                    .iter()
                    .filter(|r| r.name != "type")
                    .map(|r| format!("    {}: object", r.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ));
        for (module, body) in files {
            Mock::given(method("GET"))
                .and(path(format!("/{module}.py")))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        server
    }

    #[tokio::test]
    async fn a_new_upstream_field_is_drift_and_known_tables_match() {
        let server = mini_sdk().await;
        let source = HttpSource {
            client: reqwest::Client::new(),
            base: server.uri(),
        };
        let report = UpstreamSurface::load(&source).await.unwrap().compare();
        let request = &report.categories["request fields"];
        assert_eq!(request.new_upstream, ["future_field".to_string()].into());
        assert!(request.local_only.contains("safeguards"));
        assert!(!request.new_upstream.contains("betas"));
        let tools =
            &report.categories["tool types (all rejected; listed when no prefix names them)"];
        assert_eq!(tools.new_upstream, ["quantum_20270101".to_string()].into());
        for name in [
            "content blocks",
            "tool fields",
            "thinking types",
            "thinking displays",
            "stop reasons",
            "stream deltas",
        ] {
            let category = &report.categories[name];
            assert!(
                category.new_upstream.is_empty(),
                "{name}: {:?}",
                category.new_upstream
            );
        }
    }

    #[tokio::test]
    async fn the_public_entry_point_fetches_and_compares() {
        let server = mini_sdk().await;
        let report = fetch_drift_report_from(&server.uri()).await.unwrap();
        assert!(report.has_drift());
    }

    #[tokio::test]
    async fn a_missing_file_or_layout_change_is_an_error() {
        let server = MockServer::start().await;
        let source = HttpSource {
            client: reqwest::Client::new(),
            base: server.uri(),
        };
        assert!(UpstreamSurface::load(&source).await.is_err());
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("nothing here"))
            .mount(&server)
            .await;
        let err = UpstreamSurface::load(&source).await.unwrap_err();
        assert!(err.to_string().contains("layout changed"));
    }
}
