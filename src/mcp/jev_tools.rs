//! MCP adapters for Jev issue routing and decision verification.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use super::{error::tool_error, server::OmniDevServer};
use crate::cli::ai::jev::{
    build_ladders, fetch_docs, fetch_input, format_output_with_cache, JevFormat,
};
use crate::jev::{
    client::JevClient,
    config::JevConfig,
    route::{
        run_route_with_reference_fetch_failures, RouteOptions, DEFAULT_CLOSE_CALL,
        DEFAULT_CLOSE_CALL_MARGIN, DEFAULT_MAX_INPUT_CHARS,
    },
    verify::{
        parse_comment_selector, run_verify, VerifyOptions, DEFAULT_COVERAGE, DEFAULT_REJECT_BELOW,
        DEFAULT_SUPPORTED,
    },
};

/// Machine-readable report format.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum JevOutputFormat {
    /// JSON (default).
    #[default]
    Json,
    /// YAML.
    Yaml,
}
impl From<JevOutputFormat> for JevFormat {
    fn from(format: JevOutputFormat) -> Self {
        match format {
            JevOutputFormat::Json => Self::Json,
            JevOutputFormat::Yaml => Self::Yaml,
        }
    }
}

/// A custom ladder loaded from a local YAML file.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LadderDefinition {
    /// Ladder name, also included in `ladders`.
    pub name: String,
    /// YAML file path on the server.
    pub path: PathBuf,
}

/// Parameters for `jev_route`; defaults match `ai jev route`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JevRouteParams {
    /// Issue references: N, #N, owner/repo#N or GitHub issue URL. Exclusive with all_open.
    #[serde(default)]
    pub issues: Vec<String>,
    /// Route every open issue in the repository instead of supplying issues.
    #[serde(default)]
    pub all_open: bool,
    /// Repository directory for bare references/all_open; defaults to server cwd.
    pub repo: Option<PathBuf>,
    /// Named ladders; defaults to `["anthropic"]`. Built-ins: anthropic, openai, gemini.
    pub ladders: Option<Vec<String>>,
    /// Custom ladder definitions loaded from local YAML files.
    #[serde(default)]
    pub ladder_definition: Vec<LadderDefinition>,
    /// Local UTF-8 draft comment paths, one issue only; never posted to GitHub.
    #[serde(default)]
    pub draft_comment: Vec<PathBuf>,
    /// Ask for per-model effort recommendations (additional Jev questions).
    #[serde(default)]
    pub effort_advice: bool,
    /// Confidence below which an answer is a close call; default 0.3.
    pub close_call: Option<f64>,
    /// Top-two probability gap below which a stage is a close call; default 0.2.
    pub close_call_margin: Option<f64>,
    /// Input character cap; default 60000. Longer input has a truncation marker.
    pub max_input_chars: Option<usize>,
    /// Allow closed issues (their completed work can bias routing).
    #[serde(default)]
    pub allow_closed: bool,
    /// Skip closed issues; exclusive with allow_closed.
    #[serde(default)]
    pub ignore_closed: bool,
    /// Bypass the shared GitHub issue cache.
    #[serde(default)]
    pub refresh: bool,
    /// Jev model override, independent of the AI backend model.
    pub jev_model: Option<String>,
    /// Report format: json (default) or yaml.
    #[serde(default)]
    pub output: JevOutputFormat,
}

impl JevRouteParams {
    fn options(&self, model: String) -> Result<RouteOptions> {
        if self.issues.is_empty() != self.all_open {
            bail!("supply issues or all_open, exclusively");
        }
        if self.allow_closed && self.ignore_closed {
            bail!("allow_closed and ignore_closed are mutually exclusive");
        }
        if !self.draft_comment.is_empty() && (self.all_open || self.issues.len() != 1) {
            bail!("draft_comment requires exactly one issue");
        }
        let opts = RouteOptions {
            model,
            draft_comments: vec![],
            effort_advice: self.effort_advice,
            close_call: self.close_call.unwrap_or(DEFAULT_CLOSE_CALL),
            close_call_margin: self.close_call_margin.unwrap_or(DEFAULT_CLOSE_CALL_MARGIN),
            max_input_chars: self.max_input_chars.unwrap_or(DEFAULT_MAX_INPUT_CHARS),
            allow_closed: self.allow_closed,
            ignore_closed: self.ignore_closed,
        };
        probability("close_call", opts.close_call)?;
        probability("close_call_margin", opts.close_call_margin)?;
        input_cap(opts.max_input_chars)?;
        Ok(opts)
    }
}

/// Parameters for `jev_verify_decision`; defaults match the CLI.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JevVerifyDecisionParams {
    /// Judged issue: N, #N, owner/repo#N or GitHub issue URL.
    pub issue: String,
    /// latest (default), numeric comment id or issue-comment URL.
    pub comment: Option<String>,
    /// Repository directory for bare issue references; defaults to server cwd.
    pub repo: Option<PathBuf>,
    /// Minimum support probability for acceptance; default 0.5.
    pub threshold: Option<f64>,
    /// Probability below which a statement rejects the comment; default 0.3.
    pub reject_below: Option<f64>,
    /// Coverage below this probability requires review; default 0.5.
    pub coverage_threshold: Option<f64>,
    /// Source character cap; default 60000.
    pub max_input_chars: Option<usize>,
    /// Bypass the shared GitHub issue cache.
    #[serde(default)]
    pub refresh: bool,
    /// Jev model override for support and coverage judgments.
    pub jev_model: Option<String>,
    /// Splitter AI model override; falls back to mcp.default_model, then backend defaults.
    /// Backend selection comes from server configuration.
    pub model: Option<String>,
    /// Report format: json (default) or yaml.
    #[serde(default)]
    pub output: JevOutputFormat,
}
impl JevVerifyDecisionParams {
    fn options(&self, model: String) -> Result<VerifyOptions> {
        let opts = VerifyOptions {
            jev_model: model,
            supported: self.threshold.unwrap_or(DEFAULT_SUPPORTED),
            reject_below: self.reject_below.unwrap_or(DEFAULT_REJECT_BELOW),
            coverage: self.coverage_threshold.unwrap_or(DEFAULT_COVERAGE),
            max_input_chars: self.max_input_chars.unwrap_or(DEFAULT_MAX_INPUT_CHARS),
        };
        probability("threshold", opts.supported)?;
        probability("reject_below", opts.reject_below)?;
        probability("coverage_threshold", opts.coverage)?;
        if opts.reject_below > opts.supported {
            bail!("reject_below must not exceed threshold");
        }
        input_cap(opts.max_input_chars)?;
        Ok(opts)
    }
}
fn probability(name: &str, value: f64) -> Result<()> {
    if !(0.0..=1.0).contains(&value) {
        bail!("{name} must be finite and between 0 and 1");
    }
    Ok(())
}
fn input_cap(cap: usize) -> Result<()> {
    if cap == 0 {
        bail!("max_input_chars must be at least 1");
    }
    Ok(())
}

fn report_result<T: Serialize>(
    report: &T,
    usage: Option<crate::github_issues::CacheUsage>,
    output: JevOutputFormat,
    failed: bool,
) -> Result<CallToolResult> {
    let text = ContentBlock::text(format_output_with_cache(report, usage, output.into())?);
    Ok(if failed {
        CallToolResult::error(vec![text])
    } else {
        CallToolResult::success(vec![text])
    })
}

#[allow(missing_docs)]
#[tool_router(router = jev_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Routes issues by stage using the existing Jev engine.
    #[tool(
        description = "Route GitHub issues to model classes for design, implementation and review. Mirrors ai jev route; returns models, usage, probabilities, close calls, dependencies and cache metadata as JSON/YAML. Supply issues OR all_open. Draft comments are local previews and never posted. Requires gh authentication and configured Jev credentials. Closed issues are refused by default. Partial issue failures retain the report with isError=true; authentication failures abort."
    )]
    pub async fn jev_route(
        &self,
        Parameters(params): Parameters<JevRouteParams>,
    ) -> Result<CallToolResult, McpError> {
        run_route(params).await.map_err(tool_error)
    }

    /// Checks a decision comment against its cited sources.
    #[tool(
        description = "Verify a GitHub decision comment against its cited issues/PRs using the configured AI splitter and Jev. Mirrors ai jev verify-decision; latest selects the most recent comment with citations. Returns accepted, rejected or needs_review plus statements, coverage, models, usage and cache metadata as JSON/YAML. These verdicts are successful tool results. Low coverage alone requires review. Checks source fidelity, not whether the decision is correct. Requires gh, Jev and AI backend credentials; backend selection is server configuration, model overrides are request-local."
    )]
    pub async fn jev_verify_decision(
        &self,
        Parameters(params): Parameters<JevVerifyDecisionParams>,
    ) -> Result<CallToolResult, McpError> {
        run_decision(params).await.map_err(tool_error)
    }
}

async fn run_route(params: JevRouteParams) -> Result<CallToolResult> {
    let mut opts = params.options(String::new())?;
    let draft_paths = params.draft_comment.clone();
    let (ladders, drafts) = tokio::task::spawn_blocking(move || {
        let names = params.ladders.unwrap_or_else(|| vec!["anthropic".into()]);
        if names.is_empty() {
            bail!("ladders must not be empty");
        }
        let definitions = params
            .ladder_definition
            .into_iter()
            .map(|d| (d.name, d.path))
            .collect::<Vec<_>>();
        let ladders = build_ladders(&names, &definitions)?;
        let drafts = params
            .draft_comment
            .iter()
            .map(|p| {
                let body = std::fs::read_to_string(p)
                    .with_context(|| format!("Failed to read draft comment {}", p.display()))?;
                if body.trim().is_empty() {
                    bail!("Draft comment {} is empty", p.display());
                }
                Ok(body)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok::<_, anyhow::Error>((ladders, drafts))
    })
    .await
    .context("Route input task panicked")??;
    opts.draft_comments = drafts;
    let env = crate::utils::settings::SettingsEnv::load();
    let mut config = JevConfig::from_env_with(&env)?;
    if let Some(model) = params.jev_model {
        config.model = model;
    }
    opts.model.clone_from(&config.model);
    let client = JevClient::from_config(&config)?;
    let bin = crate::pr_status::resolve_gh_binary();
    let cache = crate::github_issues::open_cache_blocking(
        env,
        dirs::cache_dir(),
        bin.clone(),
        params.refresh,
    )
    .await?;
    let fetch_cache = std::sync::Arc::clone(&cache);
    let cwd = params.repo.unwrap_or_else(|| PathBuf::from("."));
    let max_chars = opts.max_input_chars;
    let drafts = opts.draft_comments.clone();
    let (docs, deps, failures) = tokio::task::spawn_blocking(move || {
        fetch_docs(
            &bin,
            &fetch_cache,
            &cwd,
            &params.issues,
            params.all_open,
            max_chars,
            params.ignore_closed,
            &drafts,
        )
    })
    .await
    .context("Issue fetch task panicked")??;
    let mut report =
        run_route_with_reference_fetch_failures(&client, &docs, &ladders, &opts, &deps, &failures)
            .await?;
    for issue in &mut report.issues {
        issue.draft_comments.clone_from(&draft_paths);
    }
    let failed = report
        .issues
        .iter()
        .any(crate::jev::route::IssueRoute::failed);
    report_result(&report, cache.usage(), params.output, failed)
}

async fn run_decision(params: JevVerifyDecisionParams) -> Result<CallToolResult> {
    let mut opts = params.options(String::new())?;
    let selector = parse_comment_selector(params.comment.as_deref().unwrap_or("latest"))?;
    let env = crate::utils::settings::SettingsEnv::load();
    let mut config = JevConfig::from_env_with(&env)?;
    if let Some(model) = params.jev_model {
        config.model = model;
    }
    opts.jev_model.clone_from(&config.model);
    let jev = JevClient::from_config(&config)?;
    let model = params
        .model
        .or_else(|| crate::utils::settings::Settings::load_mcp().default_model);
    let ai = crate::claude::create_default_claude_client(model, None).await?;
    let bin = crate::pr_status::resolve_gh_binary();
    let cache = crate::github_issues::open_cache_blocking(
        env,
        dirs::cache_dir(),
        bin.clone(),
        params.refresh,
    )
    .await?;
    let fetch_cache = std::sync::Arc::clone(&cache);
    let cwd = params.repo.unwrap_or_else(|| PathBuf::from("."));
    let input = tokio::task::spawn_blocking(move || {
        fetch_input(&bin, &fetch_cache, &cwd, &params.issue, &selector)
    })
    .await
    .context("Issue fetch task panicked")??;
    let (issue, comment, citations, sources) = input;
    let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts).await?;
    report_result(&report, cache.usage(), params.output, false)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(value: serde_json::Value) -> JevRouteParams {
        serde_json::from_value(value).unwrap()
    }
    fn verify(value: serde_json::Value) -> JevVerifyDecisionParams {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn defaults_match_cli_engine_defaults() {
        let p = route(json!({"issues": ["#1779"]}));
        let o = p.options("jev-test".into()).unwrap();
        assert_eq!(o.model, "jev-test");
        assert_eq!(o.close_call, DEFAULT_CLOSE_CALL);
        assert_eq!(o.close_call_margin, DEFAULT_CLOSE_CALL_MARGIN);
        assert_eq!(o.max_input_chars, DEFAULT_MAX_INPUT_CHARS);
        assert!(!o.allow_closed && !o.ignore_closed && !o.effort_advice);
        assert!(matches!(p.output, JevOutputFormat::Json));
        let o = verify(json!({"issue": "#1779"}))
            .options("jev-test".into())
            .unwrap();
        assert_eq!(o.jev_model, "jev-test");
        assert_eq!(o.supported, DEFAULT_SUPPORTED);
        assert_eq!(o.reject_below, DEFAULT_REJECT_BELOW);
        assert_eq!(o.coverage, DEFAULT_COVERAGE);
        assert_eq!(o.max_input_chars, DEFAULT_MAX_INPUT_CHARS);
    }

    #[test]
    fn route_maps_overrides_and_rejects_conflicts() {
        let p = route(json!({"issues": ["o/r#1"], "repo": "/tmp/repo",
            "ladders": ["openai", "custom"],
            "ladder_definition": [{"name": "custom", "path": "/tmp/tiers.yaml"}],
            "draft_comment": ["/tmp/draft.md"], "effort_advice": true,
            "close_call": 0.7, "close_call_margin": 0.25, "max_input_chars": 123,
            "allow_closed": true, "refresh": true, "jev_model": "jev-1.13.0", "output": "yaml"}));
        let o = p.options(p.jev_model.clone().unwrap()).unwrap();
        assert_eq!(o.close_call, 0.7);
        assert_eq!(o.close_call_margin, 0.25);
        assert_eq!(o.max_input_chars, 123);
        assert!(o.allow_closed && o.effort_advice && p.refresh);
        assert_eq!(p.repo, Some(PathBuf::from("/tmp/repo")));
        assert_eq!(p.ladders.unwrap(), ["openai", "custom"]);
        assert_eq!(p.ladder_definition[0].name, "custom");
        assert_eq!(p.draft_comment, [PathBuf::from("/tmp/draft.md")]);
        for value in [
            json!({}),
            json!({"issues":["#1"],"all_open":true}),
            json!({"issues":["#1"],"allow_closed":true,"ignore_closed":true}),
            json!({"issues":["#1"],"max_input_chars":0}),
            json!({"issues":["#1"],"close_call":1.1}),
            json!({"issues":["#1"],"close_call_margin":-0.1}),
            json!({"all_open":true,"draft_comment":["/tmp/draft"]}),
            json!({"issues":["#1","#2"],"draft_comment":["/tmp/draft"]}),
        ] {
            assert!(route(value).options("test".into()).is_err());
        }
        assert!(route(json!({"all_open":true}))
            .options("test".into())
            .is_ok());
    }

    #[test]
    fn verification_maps_models_and_thresholds_independently() {
        let p = verify(json!({"issue":"o/r#1", "comment":"123", "threshold":0.8,
            "reject_below":0.2,"coverage_threshold":0.6,"max_input_chars":20,
            "repo":"/tmp/repo","jev_model":"jev-test","model":"ai-test","refresh":true,"output":"yaml"}));
        let o = p.options(p.jev_model.clone().unwrap()).unwrap();
        assert_eq!(o.jev_model, "jev-test");
        assert_eq!(p.model.as_deref(), Some("ai-test"));
        assert_eq!(p.comment.as_deref(), Some("123"));
        assert_eq!(o.supported, 0.8);
        assert_eq!(o.reject_below, 0.2);
        assert_eq!(o.coverage, 0.6);
        assert_eq!(o.max_input_chars, 20);
        for field in ["threshold", "reject_below", "coverage_threshold"] {
            for value in [-0.1, 1.1] {
                let mut input = json!({"issue":"#1"});
                input[field] = json!(value);
                assert!(verify(input).options("test".into()).is_err());
            }
        }
        assert!(
            verify(json!({"issue":"#1","threshold":0.2,"reject_below":0.3}))
                .options("test".into())
                .is_err()
        );
        assert!(verify(json!({"issue":"#1","max_input_chars":0}))
            .options("test".into())
            .is_err());
        assert!(probability("test", f64::NAN).is_err());
    }

    #[tokio::test]
    async fn invalid_handler_inputs_fail_before_credentials_or_network() {
        let server = OmniDevServer::new();
        let err = server
            .jev_route(Parameters(route(json!({}))))
            .await
            .unwrap_err();
        assert!(err.message.contains("supply issues"));
        let err = server
            .jev_verify_decision(Parameters(verify(json!({"issue":"#1","threshold":-1}))))
            .await
            .unwrap_err();
        assert!(err.message.contains("threshold"));
        let err = server
            .jev_verify_decision(Parameters(verify(json!({"issue":"#1","comment":"bad"}))))
            .await
            .unwrap_err();
        assert!(err.message.contains("comment"));
    }

    #[test]
    fn reports_preserve_data_cache_metadata_and_error_semantics() {
        for failed in [false, true] {
            let result = report_result(
                &json!({"verdict":"rejected","models":{"jev":"test"},"usage":{"input_tokens":5}}),
                Some(crate::github_issues::CacheUsage {
                    items_reused: 2,
                    oldest_age_secs: 3,
                }),
                JevOutputFormat::Json,
                failed,
            )
            .unwrap();
            assert_eq!(result.is_error, Some(failed));
            let ContentBlock::Text(text) = &result.content[0] else {
                panic!("text");
            };
            let report: serde_json::Value = serde_json::from_str(&text.text).unwrap();
            assert_eq!(report["verdict"], "rejected");
            assert_eq!(report["models"]["jev"], "test");
            assert_eq!(report["usage"]["input_tokens"], 5);
            assert_eq!(report["github_cache"]["items_reused"], 2);
        }
        let result = report_result(
            &json!({"verdict":"needs_review"}),
            None,
            JevOutputFormat::Yaml,
            false,
        )
        .unwrap();
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("text");
        };
        let report: serde_json::Value = serde_yaml::from_str(&text.text).unwrap();
        assert_eq!(report["verdict"], "needs_review");
        assert!(report.get("github_cache").is_none());
    }

    #[test]
    fn rejects_unknown_options_instead_of_ignoring_them() {
        assert!(serde_json::from_value::<JevRouteParams>(
            json!({"issues":["#1"], "all_opne":true})
        )
        .is_err());
        assert!(serde_json::from_value::<JevVerifyDecisionParams>(
            json!({"issue":"#1", "ai_backend":"ollama"})
        )
        .is_err());
    }

    #[tokio::test]
    async fn invalid_ladders_and_drafts_fail_before_credentials_or_network() {
        let server = OmniDevServer::new();
        for value in [
            json!({"issues":["#1"],"ladders":[]}),
            json!({"issues":["#1"],"ladders":["missing"]}),
        ] {
            let err = server
                .jev_route(Parameters(route(value)))
                .await
                .unwrap_err();
            assert!(err.message.contains("ladder"), "{err}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.md");
        std::fs::write(&path, "  ").unwrap();
        let err = server
            .jev_route(Parameters(route(
                json!({"issues":["#1"],"draft_comment":[path]}),
            )))
            .await
            .unwrap_err();
        assert!(err.message.contains("empty"), "{err}");
    }

    #[test]
    fn tools_are_in_combined_router() {
        let server = OmniDevServer::new();
        for name in ["jev_route", "jev_verify_decision"] {
            assert!(server.tool_router.has_route(name));
        }
    }
}
