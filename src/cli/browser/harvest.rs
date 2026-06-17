//! `omni-dev browser bridge harvest <platform> <object>` — best-effort
//! harvesters that page a logged-in tab's **own** data out through the bridge.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::browser::bridge::DEFAULT_CONTROL_PORT;
use crate::browser::harvest::facebook;

/// Harvest a logged-in browser session's own data through the bridge.
///
/// Harvesters drive reverse-engineered, undocumented site internals and are
/// BEST-EFFORT: they may break whenever the target site changes its query ids,
/// page structure, or response shape. They only ever use the session already
/// logged into the connected tab (your own account).
#[derive(Parser)]
pub struct HarvestCommand {
    /// The platform to harvest from.
    #[command(subcommand)]
    pub platform: HarvestPlatform,
}

/// Supported harvest platforms.
#[derive(Subcommand)]
pub enum HarvestPlatform {
    /// Harvest your own data from Facebook.
    Facebook(FacebookCommand),
}

impl HarvestCommand {
    /// Executes the harvest command.
    pub async fn execute(self) -> Result<()> {
        match self.platform {
            HarvestPlatform::Facebook(cmd) => cmd.execute().await,
        }
    }
}

/// Harvest your own data from Facebook.
#[derive(Parser)]
pub struct FacebookCommand {
    /// The object to harvest.
    #[command(subcommand)]
    pub object: FacebookObject,
}

/// Facebook objects that can be harvested.
#[derive(Subcommand)]
pub enum FacebookObject {
    /// Download your own timeline posts.
    Posts(PostsCommand),
}

impl FacebookCommand {
    /// Executes the Facebook harvest command.
    pub async fn execute(self) -> Result<()> {
        match self.object {
            FacebookObject::Posts(cmd) => cmd.execute().await,
        }
    }
}

/// Output serialisation for harvested posts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// One JSON object per line (default; streamed and resume-friendly).
    Jsonl,
    /// A single JSON array, written once when the run completes.
    Json,
}

impl From<Format> for facebook::Format {
    fn from(f: Format) -> Self {
        match f {
            Format::Jsonl => Self::Jsonl,
            Format::Json => Self::Json,
        }
    }
}

/// Download the signed-in user's own Facebook timeline posts through the bridge.
///
/// Encapsulates the manual recipe (issue #922): harvest session tokens from the
/// `/me` shell, discover the pagination `doc_id` from a cross-origin script
/// bundle (requires the tab's bridge to permit `https://static.xx.fbcdn.net`),
/// then replay the refetch GraphQL query, paging to the end of the timeline.
///
/// BEST-EFFORT: this drives undocumented Facebook internals and may break when
/// Facebook changes its GraphQL `doc_id`s, page structure, or response shape.
/// No `doc_id`, token, or provider flag is hardcoded — all are re-harvested
/// every run. For a stable archive, use Facebook's official "Download Your
/// Information" export instead. Your own account only.
#[derive(Parser)]
pub struct PostsCommand {
    /// Control-plane port of the running bridge.
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    pub control_port: u16,

    /// Read the session token from this `0600` file instead of the environment.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,

    /// Route to a specific connected tab: a connection id (from
    /// `/__bridge/status`) or an `Origin` that uniquely matches one tab.
    /// Required when more than one tab is connected.
    #[arg(long, value_name = "ID|ORIGIN")]
    pub target: Option<String>,

    /// Write posts to this file instead of stdout.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Output format: one post per line (`jsonl`) or a single array (`json`).
    #[arg(long, value_enum, default_value_t = Format::Jsonl)]
    pub format: Format,

    /// Stop paging once posts are older than this (Unix seconds or ISO-8601).
    #[arg(long, value_name = "UNIX-TS|ISO8601")]
    pub since: Option<String>,

    /// Stop after this many posts (smoke tests / sampling).
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Continue a prior interrupted run from its saved cursor (state file path).
    #[arg(long, value_name = "PATH")]
    pub resume: Option<PathBuf>,
}

impl PostsCommand {
    /// Executes the Facebook posts harvest.
    pub async fn execute(self) -> Result<()> {
        tracing::warn!(
            "BEST-EFFORT: drives undocumented Facebook internals and may break when Facebook \
             changes. Your own account only. Stable alternative: Facebook's \"Download Your \
             Information\" export."
        );
        let token = super::resolve_client_token(self.token_file.as_deref())?;
        let since = self.since.as_deref().map(parse_since).transpose()?;
        let output = match self.output {
            Some(path) => facebook::Output::File(path),
            None => facebook::Output::Stdout,
        };
        let config = facebook::HarvestConfig {
            control_port: self.control_port,
            token,
            target: self.target,
            output,
            format: self.format.into(),
            since,
            limit: self.limit,
            resume: self.resume,
        };
        facebook::run(config).await
    }
}

/// Parses a `--since` value: bare digits are a Unix timestamp; anything else is
/// parsed as an RFC-3339 / ISO-8601 datetime and converted to Unix seconds.
fn parse_since(raw: &str) -> Result<i64> {
    let raw = raw.trim();
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        return raw
            .parse::<i64>()
            .with_context(|| format!("invalid --since Unix timestamp: {raw}"));
    }
    let dt = chrono::DateTime::parse_from_rfc3339(raw).with_context(|| {
        format!("invalid --since timestamp (expected Unix seconds or ISO-8601): {raw}")
    })?;
    Ok(dt.timestamp())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> PostsCommand {
        let mut full = vec!["posts"];
        full.extend_from_slice(args);
        PostsCommand::try_parse_from(full).unwrap()
    }

    #[test]
    fn defaults_match_documented_values() {
        let c = parse(&[]);
        assert_eq!(c.control_port, 9998);
        assert_eq!(c.format, Format::Jsonl);
        assert!(c.target.is_none());
        assert!(c.output.is_none());
        assert!(c.since.is_none());
        assert!(c.limit.is_none());
        assert!(c.resume.is_none());
    }

    #[test]
    fn flags_are_parsed() {
        let c = parse(&[
            "--control-port",
            "9000",
            "--target",
            "1",
            "--output",
            "/tmp/p.jsonl",
            "--format",
            "json",
            "--since",
            "1700000000",
            "--limit",
            "5",
            "--resume",
            "/tmp/state.json",
        ]);
        assert_eq!(c.control_port, 9000);
        assert_eq!(c.target.as_deref(), Some("1"));
        assert_eq!(c.format, Format::Json);
        assert_eq!(c.limit, Some(5));
    }

    #[test]
    fn format_rejects_unknown_value() {
        assert!(PostsCommand::try_parse_from(["posts", "--format", "csv"]).is_err());
    }

    #[test]
    fn parse_since_accepts_unix_and_iso() {
        assert_eq!(parse_since("1700000000").unwrap(), 1_700_000_000);
        assert_eq!(parse_since("2023-11-14T22:13:20Z").unwrap(), 1_700_000_000);
        assert!(parse_since("not-a-date").is_err());
    }

    #[test]
    fn format_converts_to_engine_format() {
        assert_eq!(
            facebook::Format::from(Format::Jsonl),
            facebook::Format::Jsonl
        );
        assert_eq!(facebook::Format::from(Format::Json), facebook::Format::Json);
    }

    /// Writes `token` to a `0600` file so `resolve_existing_token` accepts it.
    fn token_file(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("token");
        std::fs::write(&path, "test-token\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    /// Builds the full `harvest facebook posts` command around a `PostsCommand`.
    fn harvest_cmd(posts: PostsCommand) -> HarvestCommand {
        HarvestCommand {
            platform: HarvestPlatform::Facebook(FacebookCommand {
                object: FacebookObject::Posts(posts),
            }),
        }
    }

    #[tokio::test]
    async fn execute_dispatches_through_to_run_and_surfaces_bridge_error() {
        let dir = tempfile::tempdir().unwrap();
        // control_port 0 never has a listener, so `run` fails fast at step 1 —
        // exercising the whole dispatch chain and config assembly without a
        // live bridge. Stdout output + jsonl format.
        let cmd = harvest_cmd(PostsCommand {
            control_port: 0,
            token_file: Some(token_file(dir.path())),
            target: None,
            output: None,
            format: Format::Jsonl,
            since: Some("1700000000".to_string()),
            limit: Some(5),
            resume: None,
        });
        let err = cmd.execute().await.unwrap_err();
        assert!(
            err.to_string().contains("Failed to reach bridge"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_with_file_output_and_json_format_takes_the_file_arm() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = harvest_cmd(PostsCommand {
            control_port: 0,
            token_file: Some(token_file(dir.path())),
            target: None,
            output: Some(dir.path().join("out.json")),
            format: Format::Json,
            since: None,
            limit: None,
            resume: None,
        });
        // Still fails at the unreachable bridge, but only after building the
        // `File` output + `json` format config.
        assert!(cmd.execute().await.is_err());
    }
}
