//! CLI text replacement; permission/revision/lease enforcement is in the engine.
use crate::cli::drive::{
    format::{output_as, sanitize_for_terminal, OutputFormat},
    helpers,
};
use crate::drive::client::DriveClient;
use crate::drive::slides::{
    client::SlidesClient,
    write::{describe, write, WriteOptions},
};
use crate::drive::write_gate::FolderPermissionRule;
use anyhow::Result;
use clap::Parser;

/// Replaces literal text on ordinary slides. Notes/templates are excluded.
#[derive(Parser)]
pub struct ReplaceCommand {
    /// Presentation id (the /d/<ID>/ segment of a Slides URL).
    pub presentation_id: String,
    /// Literal text to find (not a regex).
    #[arg(long)]
    pub search: String,
    /// Replacement text; may be empty to remove matches.
    #[arg(long)]
    pub replace: String,
    /// Match case-insensitively. Matching is case-sensitive by default.
    #[arg(long)]
    pub ignore_case: bool,
    /// Restrict to an ordinary slide object ID. Repeat for multiple slides.
    #[arg(long = "slide", value_name = "OBJECT_ID")]
    pub slides: Vec<String>,
    /// Report snapshot estimates without mutating or consuming a lease.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub lease: helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}
impl ReplaceCommand {
    /// Builds options and delegates to the guarded runner.
    pub async fn execute(self, drive: &DriveClient) -> Result<()> {
        let slides = SlidesClient::from_drive_client(drive)?;
        let opts = WriteOptions {
            presentation_id: self.presentation_id,
            search: self.search,
            replace: self.replace,
            match_case: !self.ignore_case,
            slides: self.slides,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        let rules = helpers::active_account_rules()?;
        run_write(drive, &slides, &opts, &rules, &self.output).await
    }
}
async fn run_write(
    drive: &DriveClient,
    slides: &SlidesClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = write(drive, slides, opts, rules).await;
    if !output_as(&outcome, output)? {
        println!("{}", sanitize_for_terminal(&describe(&outcome)));
    }
    Ok(())
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn defaults_to_case_sensitive_and_accepts_repeated_slide_filters() {
        let cmd = ReplaceCommand::try_parse_from([
            "replace",
            "p",
            "--search",
            "a",
            "--replace",
            "",
            "--slide",
            "s1",
            "--slide",
            "s2",
        ])
        .unwrap();
        assert!(!cmd.ignore_case);
        assert_eq!(cmd.slides, ["s1", "s2"]);
        assert!(
            ReplaceCommand::try_parse_from([
                "replace",
                "p",
                "--search",
                "a",
                "--replace",
                "b",
                "--ignore-case"
            ])
            .unwrap()
            .ignore_case
        );
    }

    use crate::drive::slides::client::test_support::mock_clients;
    use crate::drive::types::GOOGLE_SLIDES_MIME_TYPE;
    use crate::drive::write_gate::DriveOperation;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn opts(dry_run: bool) -> WriteOptions {
        WriteOptions {
            presentation_id: "p1".into(),
            search: "Q3".into(),
            replace: "Q4".into(),
            match_case: true,
            slides: vec![],
            dry_run,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        }
    }
    fn allow_p1() -> FolderPermissionRule {
        FolderPermissionRule {
            file_id: Some("p1".into()),
            folder_id: None,
            recursive: false,
            allow: std::iter::once(DriveOperation::SlidesWrite).collect(),
            deny: std::collections::HashSet::default(),
            require_lease: false,
        }
    }
    async fn serve_file(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"p1","name":"Deck","mimeType":GOOGLE_SLIDES_MIME_TYPE,"version":"1"
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_write_renders_a_dry_run_in_every_format_without_mutating() {
        for output in [
            OutputFormat::Table,
            OutputFormat::Json,
            OutputFormat::Yaml,
            OutputFormat::Jsonl,
        ] {
            let server = MockServer::start().await;
            let (drive, slides) = mock_clients(&server).await;
            serve_file(&server).await;
            Mock::given(method("GET"))
                .and(path("/v1/presentations/p1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "revisionId":"rev-1",
                    "slides":[{"objectId":"s1","pageElements":[{"objectId":"e","shape":{"text":{"textElements":[{"textRun":{"content":"Q3"}}]}}}]}]
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/v1/presentations/p1:batchUpdate"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            run_write(&drive, &slides, &opts(true), &[allow_p1()], &output)
                .await
                .unwrap();
        }
    }
    #[tokio::test]
    async fn run_write_reports_a_refusal_as_output_rather_than_an_error() {
        let server = MockServer::start().await;
        let (drive, slides) = mock_clients(&server).await;
        serve_file(&server).await;
        Mock::given(path("/v1/presentations/p1"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        // No rules and no parents: refused before any Slides read.
        run_write(&drive, &slides, &opts(true), &[], &OutputFormat::Table)
            .await
            .unwrap();
    }
}
