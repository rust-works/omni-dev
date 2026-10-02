//! `ai jev exists` — one-round, body-free code existence screening.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use super::common::{format_output, JevFormat};
use crate::jev::{
    client::JevClient,
    config::JevConfig,
    exists::{build_request, judge, retrieve, ExistsReport, Retrieval, MAX_ISSUE_BYTES},
};
use crate::utils::env::EnvSource;

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

fn load_issue_text(
    gh: &Path,
    repo: &Path,
    issue: Option<String>,
    file: Option<PathBuf>,
) -> Result<String> {
    if let Some(file) = file {
        let mut bytes = Vec::new();
        std::fs::File::open(&file)
            .with_context(|| format!("read issue file {}", file.display()))?
            .take((MAX_ISSUE_BYTES + 4) as u64)
            .read_to_end(&mut bytes)?;
        // The final character may have been cut by the read bound.
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text.to_owned(),
            Err(error) if error.error_len().is_none() && bytes.len() == MAX_ISSUE_BYTES + 4 => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])?.to_owned()
            }
            Err(error) => return Err(error.into()),
        };
        Ok(text)
    } else {
        let raw = issue.context("issue required")?;
        let default = if crate::github_issues::needs_default_project(&raw) {
            Some(crate::github_issues::resolve_current_project(gh, repo)?)
        } else {
            None
        };
        let item = crate::github_issues::parse_issue_arg(&raw, default.as_deref())?;
        let docs = crate::github_issues::fetch_issues(gh, &[item])?;
        let doc = docs.first().context("issue fetch returned no document")?;
        Ok(format!("{}\n\n{}", doc.title, doc.body))
    }
}

/// Renders the paths that never call Jev: a dry run (the retrieval plus the
/// exact request, never sent) and a retrieval with nothing to judge. Neither
/// needs credentials, so neither builds a client.
fn render_without_call(
    retrieval: Retrieval,
    dry_run: bool,
    model: &str,
    output: JevFormat,
) -> Result<String> {
    if dry_run {
        let request = if retrieval.candidates.is_empty() {
            None
        } else {
            Some(build_request(&retrieval, model)?)
        };
        format_output(
            &serde_json::json!({"retrieval":retrieval,"request":request}),
            output,
        )
    } else {
        format_output(
            &ExistsReport {
                retrieval,
                model: None,
                usage: None,
            },
            output,
        )
    }
}

impl ExistsCommand {
    /// Retrieves candidates and asks one Jev round, unless none were found.
    ///
    /// `gh` and `env` are injected so tests need neither a real `gh` login
    /// nor process-global state (STYLE-0025, STYLE-0028).
    async fn render(self, gh: PathBuf, env: &(impl EnvSource + Sync)) -> Result<String> {
        let repo = self
            .repo
            .path()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let (issue, file) = (self.issue, self.issue_file);
        let retrieval = tokio::task::spawn_blocking(move || {
            let text = load_issue_text(&gh, &repo, issue, file)?;
            retrieve(&repo, &text)
        })
        .await
        .context("existence retrieval task panicked")??;
        if self.dry_run || retrieval.candidates.is_empty() {
            let model = self
                .jev_model
                .as_deref()
                .unwrap_or(crate::jev::protocol::DEFAULT_MODEL);
            return render_without_call(retrieval, self.dry_run, model, self.output);
        }
        let mut config = JevConfig::from_env_with(env)?;
        if let Some(model) = self.jev_model {
            config.model = model;
        }
        let client = JevClient::from_config(&config)?;
        format_output(
            &judge(retrieval, &client, &config.model).await?,
            self.output,
        )
    }

    /// Runs the command against the real `gh` binary and process environment.
    pub async fn execute(self) -> Result<()> {
        let env = crate::utils::settings::SettingsEnv::load();
        let output = self
            .render(crate::pr_status::resolve_gh_binary(), &env)
            .await?;
        print!("{output}");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;
    use crate::test_support::git_repo::commit_files;
    use crate::test_support::shim::{retry_on_etxtbsy, shim_lock, write_exec_script};

    const NO_GH: &str = "/no/such/gh/xyzzy";

    fn command(args: &[&str]) -> ExistsCommand {
        ExistsCommand::try_parse_from(std::iter::once("exists").chain(args.iter().copied()))
            .unwrap()
    }

    /// A repository whose one committed function is `caller`, plus the path of
    /// an issue file naming it.
    fn repo_and_issue(issue: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = commit_files(&[("walk.rs", b"/// Walks keys.\nfn caller() {}\n")]);
        let file = dir.path().join("issue.txt");
        std::fs::write(&file, issue).unwrap();
        (dir, file)
    }

    fn args<'a>(dir: &'a tempfile::TempDir, file: &'a Path, extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec![
            "--issue-file",
            file.to_str().unwrap(),
            "-C",
            dir.path().to_str().unwrap(),
        ];
        args.extend(extra);
        args
    }

    /// A fake `gh` answering `repo view` and `api graphql` with one issue, and
    /// logging each invocation's first argument to `calls`.
    fn fake_gh(dir: &Path) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let issue = serde_json::json!({
            "title": "Add a key walk", "body": "Reuse `caller`.", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let graphql = serde_json::json!({"data": {"r0": {"i0": issue}}});
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\necho \"$1\" >> '{calls}'\ncase \"$1\" in\n\
                 repo) echo rust-works/omni-dev ;;\n\
                 api) cat <<'JSON'\n{graphql}\nJSON\n;;\n\
                 esac\n",
                calls = dir.join("calls").display()
            ),
        );
        (path, guard)
    }

    fn calls(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn issue_file_rejects_invalid_utf8_and_bounds_reads() {
        let gh = Path::new(NO_GH);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("issue.txt");
        std::fs::write(&file, [b'a', 0xc3]).unwrap();
        assert!(load_issue_text(gh, dir.path(), None, Some(file.clone())).is_err());
        std::fs::write(&file, "é".repeat(MAX_ISSUE_BYTES)).unwrap();
        let text = load_issue_text(gh, dir.path(), None, Some(file)).unwrap();
        assert!(text.len() <= MAX_ISSUE_BYTES + 4);
        assert!(load_issue_text(gh, dir.path(), None, Some(dir.path().join("missing"))).is_err());
    }

    #[test]
    fn a_character_cut_by_the_read_bound_is_dropped_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("issue.txt");
        // The read stops one byte into the two-byte `é`.
        std::fs::write(&file, format!("{}é", "a".repeat(MAX_ISSUE_BYTES + 3))).unwrap();
        let text = load_issue_text(Path::new(NO_GH), dir.path(), None, Some(file)).unwrap();
        assert_eq!(text, "a".repeat(MAX_ISSUE_BYTES + 3));
    }

    #[test]
    fn a_bare_number_resolves_the_current_project_before_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let (gh, _shim) = fake_gh(dir.path());
        let text =
            retry_on_etxtbsy(|| load_issue_text(&gh, dir.path(), Some("5".into()), None)).unwrap();
        assert_eq!(text, "Add a key walk\n\nReuse `caller`.");
        assert_eq!(calls(dir.path()), ["repo", "api"]);
    }

    #[test]
    fn a_qualified_issue_skips_project_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let (gh, _shim) = fake_gh(dir.path());
        let text = retry_on_etxtbsy(|| {
            load_issue_text(&gh, dir.path(), Some("rust-works/omni-dev#5".into()), None)
        })
        .unwrap();
        assert_eq!(text, "Add a key walk\n\nReuse `caller`.");
        assert_eq!(calls(dir.path()), ["api"]);
    }

    #[test]
    fn an_issue_or_a_file_is_required_by_the_loader_too() {
        let err = load_issue_text(Path::new(NO_GH), Path::new("."), None, None).unwrap_err();
        assert!(err.to_string().contains("issue required"));
    }

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

    #[tokio::test]
    async fn dry_run_shows_the_exact_request_without_credentials() {
        let (dir, file) = repo_and_issue("Reuse `caller`.");
        let out = command(&args(&dir, &file, &["--dry-run", "--jev-model", "pinned"]))
            .render(NO_GH.into(), &MapEnv::new())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["request"]["model"], "pinned");
        assert!(value["request"]["questions"]["candidate_0"].is_object());
        assert_eq!(value["retrieval"]["candidates"][0]["symbol"], "caller");
        let out = command(&args(&dir, &file, &["--dry-run"]))
            .render(NO_GH.into(), &MapEnv::new())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value["request"]["model"],
            crate::jev::protocol::DEFAULT_MODEL
        );
    }

    #[tokio::test]
    async fn dry_run_without_candidates_has_no_request() {
        let (dir, file) = repo_and_issue("Reuse `missing`.");
        let out = command(&args(&dir, &file, &["--dry-run"]))
            .render(NO_GH.into(), &MapEnv::new())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(value["request"].is_null());
        assert_eq!(value["retrieval"]["status"], "no_hits");
    }

    #[tokio::test]
    async fn no_candidates_report_without_calling_jev_or_needing_credentials() {
        let (dir, file) = repo_and_issue("Reuse `missing`.");
        let out = command(&args(&dir, &file, &["-o", "yaml"]))
            .render(NO_GH.into(), &MapEnv::new())
            .await
            .unwrap();
        assert!(out.contains("status: no_hits"));
        assert!(out.contains("model: null"));
        assert!(out.contains("usage: null"));
    }

    #[tokio::test]
    async fn candidates_are_judged_in_one_round_with_the_pinned_model() {
        let (dir, file) = repo_and_issue("Reuse `caller`.");
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "pinned"}),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "actual",
                    "answers": {"candidate_0": {"type": "noul", "noul": 0.9}},
                    "usage": {"input_tokens": 100, "output_tokens": 10}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let env = MapEnv::new()
            .with("TYPESAFE_API_KEY", "test-key")
            .with("OMNI_DEV_JEV_BASE_URL", &server.uri());
        let out = command(&args(&dir, &file, &["--jev-model", "pinned", "-o", "yaml"]))
            .render(NO_GH.into(), &env)
            .await
            .unwrap();
        assert!(out.contains("score: 0.9"));
        assert!(out.contains("model: actual"));
    }

    #[tokio::test]
    async fn the_configured_model_is_used_without_an_override() {
        let (dir, file) = repo_and_issue("Reuse `caller`.");
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::body_partial_json(
            serde_json::json!({"model": "configured"}),
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "configured",
                "answers": {"candidate_0": {"type": "noul", "noul": 0.1}},
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })),
        )
        .expect(1)
        .mount(&server)
        .await;
        let env = MapEnv::new()
            .with("TYPESAFE_API_KEY", "test-key")
            .with("TYPESAFE_MODEL", "configured")
            .with("OMNI_DEV_JEV_BASE_URL", &server.uri());
        let out = command(&args(&dir, &file, &[]))
            .render(NO_GH.into(), &env)
            .await
            .unwrap();
        assert!(out.contains("\"score\": 0.1"));
    }

    #[tokio::test]
    async fn judging_needs_credentials_and_surfaces_api_errors() {
        let (dir, file) = repo_and_issue("Reuse `caller`.");
        let err = command(&args(&dir, &file, &[]))
            .render(NO_GH.into(), &MapEnv::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("TYPESAFE_API_KEY"));
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server)
            .await;
        let env = MapEnv::new()
            .with("TYPESAFE_API_KEY", "test-key")
            .with("OMNI_DEV_JEV_BASE_URL", &server.uri());
        let err = command(&args(&dir, &file, &[]))
            .render(NO_GH.into(), &env)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 400"));
    }

    #[tokio::test]
    async fn retrieval_failures_propagate() {
        let not_a_repo = tempfile::tempdir().unwrap();
        let file = not_a_repo.path().join("issue.txt");
        std::fs::write(&file, "Reuse `caller`.").unwrap();
        let err = command(&args(&not_a_repo, &file, &["--dry-run"]))
            .render(NO_GH.into(), &MapEnv::new())
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
