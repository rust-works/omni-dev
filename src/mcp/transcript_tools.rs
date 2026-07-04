//! MCP tool handlers for fetching YouTube transcripts.
//!
//! Read-only content fetch: each tool constructs a [`Youtube`] source and calls
//! the same [`TranscriptSource`] trait methods the CLI uses
//! (`src/cli/transcript/youtube/`), returning the structured result as text.
//! The `sync` subcommand (which mutates local caches) has no MCP form and is
//! intentionally absent.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::server::OmniDevServer;
use crate::transcript::format::Format;
use crate::transcript::source::{FetchOpts, TranscriptSource};
use crate::transcript::sources::youtube::Youtube;

/// Rendering for a fetched transcript.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptFormat {
    /// Plain text — cue text only, one cue per line (default; most readable).
    #[default]
    Txt,
    /// SubRip (`.srt`), with timing.
    Srt,
    /// WebVTT (`.vtt`), with timing.
    Vtt,
    /// JSON — the full transcript struct (cues + metadata).
    Json,
}

impl From<TranscriptFormat> for Format {
    fn from(value: TranscriptFormat) -> Self {
        match value {
            TranscriptFormat::Txt => Self::Txt,
            TranscriptFormat::Srt => Self::Srt,
            TranscriptFormat::Vtt => Self::Vtt,
            TranscriptFormat::Json => Self::Json,
        }
    }
}

/// Parameters for the `transcript_youtube_fetch` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TranscriptFetchParams {
    /// YouTube video URL or bare 11-character video ID.
    pub url: String,
    /// Preferred caption language (e.g. `en`, `en-US`). Prefix fallback applies
    /// (`en` matches `en-US`). Defaults to `en`.
    #[serde(default = "default_lang")]
    pub lang: String,
    /// Output rendering. Defaults to `txt`.
    #[serde(default)]
    pub format: TranscriptFormat,
    /// Allow falling through to auto-generated (ASR) captions when no manual
    /// track matches. Defaults to `false`.
    #[serde(default)]
    pub auto: bool,
    /// Synthesise a translated track in this target language when no native
    /// track matches.
    #[serde(default)]
    pub translate: Option<String>,
}

fn default_lang() -> String {
    "en".to_string()
}

/// Parameters for the `transcript_youtube_info` and
/// `transcript_youtube_list_langs` tools.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TranscriptLocatorParams {
    /// YouTube video URL or bare 11-character video ID.
    pub url: String,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `transcript_tool_router` fn.
#[tool_router(router = transcript_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: fetch a YouTube transcript and render it.
    #[tool(
        description = "Fetch the transcript for a YouTube video and return it rendered as text. \
                       Read-only. Mirrors `omni-dev transcript youtube fetch`. `format` is `txt` \
                       (default, one cue per line), `srt`, `vtt`, or `json`. `lang` defaults to \
                       `en` (prefix fallback: `en` matches `en-US`). Set `auto = true` to allow \
                       auto-generated (ASR) captions when no manual track matches; set `translate` \
                       to request a machine-translated track in that language."
    )]
    pub async fn transcript_youtube_fetch(
        &self,
        Parameters(params): Parameters<TranscriptFetchParams>,
    ) -> Result<CallToolResult, McpError> {
        let rendered = run_fetch(params).await.map_err(tool_error)?;
        Ok(build_truncated_result(rendered))
    }

    /// Tool: fetch top-level metadata about a YouTube video.
    #[tool(
        description = "Fetch top-level metadata about a YouTube video (title, author, duration, \
                       available caption tracks) as YAML. Read-only. Mirrors \
                       `omni-dev transcript youtube info`."
    )]
    pub async fn transcript_youtube_info(
        &self,
        Parameters(params): Parameters<TranscriptLocatorParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_info(&params.url).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list the caption tracks available on a YouTube video.
    #[tool(
        description = "List the caption tracks available on a YouTube video (code, name, and \
                       whether each is manual, auto-generated, or translated) as YAML. Read-only. \
                       Mirrors `omni-dev transcript youtube list-langs`."
    )]
    pub async fn transcript_youtube_list_langs(
        &self,
        Parameters(params): Parameters<TranscriptLocatorParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_list_langs(&params.url).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }
}

/// Fetches a transcript and renders it in the requested format. Shares the
/// `Youtube` source and `Format::render` with the CLI's `fetch` command.
async fn run_fetch(params: TranscriptFetchParams) -> Result<String> {
    let yt = Youtube::new()?;
    let opts = FetchOpts {
        language: params.lang,
        allow_auto: params.auto,
        translate_to: params.translate,
    };
    let transcript = yt.fetch(&params.url, &opts).await?;
    Format::from(params.format)
        .render(&transcript)
        .context("Failed to render transcript")
}

/// Fetches media metadata and serializes it as YAML. `?` converts the
/// transcript-specific error into `anyhow` at the boundary.
async fn run_info(url: &str) -> Result<String> {
    let yt = Youtube::new()?;
    let info = yt.info(url).await?;
    serde_yaml::to_string(&info).context("Failed to serialize media info as YAML")
}

/// Lists available caption tracks and serializes them as YAML.
async fn run_list_langs(url: &str) -> Result<String> {
    let yt = Youtube::new()?;
    let langs = yt.list_languages(url).await?;
    serde_yaml::to_string(&langs).context("Failed to serialize languages as YAML")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn fetch_params_require_url_and_default_lang() {
        assert!(serde_json::from_str::<TranscriptFetchParams>("{}").is_err());
        let p: TranscriptFetchParams = serde_json::from_str(r#"{"url": "abc"}"#).unwrap();
        assert_eq!(p.url, "abc");
        assert_eq!(p.lang, "en");
        assert!(matches!(p.format, TranscriptFormat::Txt));
        assert!(!p.auto);
    }

    #[test]
    fn locator_params_require_url() {
        assert!(serde_json::from_str::<TranscriptLocatorParams>("{}").is_err());
        let p: TranscriptLocatorParams = serde_json::from_str(r#"{"url": "abc"}"#).unwrap();
        assert_eq!(p.url, "abc");
    }

    #[test]
    fn format_maps_to_library_format() {
        assert_eq!(Format::from(TranscriptFormat::Txt), Format::Txt);
        assert_eq!(Format::from(TranscriptFormat::Srt), Format::Srt);
        assert_eq!(Format::from(TranscriptFormat::Vtt), Format::Vtt);
        assert_eq!(Format::from(TranscriptFormat::Json), Format::Json);
    }
}
