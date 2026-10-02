//! `ai jev exists` — one-round, body-free code existence screening.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use super::common::{format_output, JevFormat};
use crate::jev::{
    client::JevClient,
    config::JevConfig,
    exists::{build_request, judge, retrieve, ExistsReport},
};

/// Screens whether local Rust definitions already provide an issue's proposed work.
#[derive(Parser)]
pub struct ExistsCommand {
    /// One issue: N, #N, owner/repo#N, or GitHub issue URL.
    #[arg(required_unless_present = "issue_file", conflicts_with = "issue_file")]
    pub issue: Option<String>,
    /// Read pinned issue text from a UTF-8 file instead of GitHub.
    #[arg(long, value_name = "FILE")]
    pub issue_file: Option<PathBuf>,
    /// Inspect the exact bounded request without calling Jev or requiring credentials.
    #[arg(long)]
    pub dry_run: bool,
    /// Local repository to search (committed HEAD only).
    #[command(flatten)]
    pub repo: crate::cli::repo_arg::RepoArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = JevFormat::Json)]
    pub(super) output: JevFormat,
    /// Override the configured Jev model.
    #[arg(long, value_name = "MODEL")]
    pub jev_model: Option<String>,
}

fn load_issue_text(repo: &Path, issue: Option<String>, file: Option<PathBuf>) -> Result<String> {
    if let Some(file) = file {
        let mut bytes = Vec::new();
        std::fs::File::open(&file)
            .with_context(|| format!("read issue file {}", file.display()))?
            .take(16 * 1024 + 4)
            .read_to_end(&mut bytes)?;
        // The final character may have been cut by the read bound.
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text.to_owned(),
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])?.to_owned()
            }
            Err(error) => return Err(error.into()),
        };
        Ok(text)
    } else {
        let bin = crate::pr_status::resolve_gh_binary();
        let raw = issue.context("issue required")?;
        let default = if crate::github_issues::needs_default_project(&raw) {
            Some(crate::github_issues::resolve_current_project(&bin, repo)?)
        } else {
            None
        };
        let item = crate::github_issues::parse_issue_arg(&raw, default.as_deref())?;
        let docs = crate::github_issues::fetch_issues(&bin, &[item])?;
        let doc = docs.first().context("issue fetch returned no document")?;
        Ok(format!("{}\n\n{}", doc.title, doc.body))
    }
}

impl ExistsCommand {
    /// Retrieves candidates and asks one Jev round, unless none were found.
    pub async fn execute(self) -> Result<()> {
        let repo = self
            .repo
            .path()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let issue = self.issue;
        let file = self.issue_file;
        let retrieval = tokio::task::spawn_blocking(move || {
            let text = load_issue_text(&repo, issue, file)?;
            retrieve(&repo, &text)
        })
        .await
        .context("existence retrieval task panicked")??;
        let output = if self.dry_run {
            let model = self
                .jev_model
                .as_deref()
                .unwrap_or(crate::jev::protocol::DEFAULT_MODEL);
            let request = if retrieval.candidates.is_empty() {
                None
            } else {
                Some(build_request(&retrieval, model)?)
            };
            format_output(
                &serde_json::json!({"retrieval":retrieval,"request":request}),
                self.output,
            )?
        } else if retrieval.candidates.is_empty() {
            format_output(
                &ExistsReport {
                    retrieval,
                    model: None,
                    usage: None,
                },
                self.output,
            )?
        } else {
            let env = crate::utils::settings::SettingsEnv::load();
            let mut config = JevConfig::from_env_with(&env)?;
            if let Some(model) = self.jev_model {
                config.model = model;
            }
            let client = JevClient::from_config(&config)?;
            format_output(
                &judge(retrieval, &client, &config.model).await?,
                self.output,
            )?
        };
        print!("{output}");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_input_and_scoped_flags_are_required() {
        assert!(ExistsCommand::try_parse_from(["exists"]).is_err());
        assert!(ExistsCommand::try_parse_from(["exists", "1", "2"]).is_err());
        assert!(
            ExistsCommand::try_parse_from(["exists", "1", "--issue-file", "input.txt"]).is_err()
        );
        assert!(ExistsCommand::try_parse_from(["exists", "1", "--model", "chat-model"]).is_err());
        assert!(ExistsCommand::try_parse_from(["exists", "1", "--ai-backend", "openai"]).is_err());
        let command = ExistsCommand::try_parse_from([
            "exists",
            "--issue-file",
            "input.txt",
            "-C",
            "/repo",
            "--dry-run",
            "-o",
            "yaml",
            "--jev-model",
            "pinned",
        ])
        .unwrap();
        assert!(command.dry_run);
        assert_eq!(command.repo.path(), Some(Path::new("/repo")));
        assert_eq!(command.output, JevFormat::Yaml);
    }
}
