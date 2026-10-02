//! CLI object-addressed presentation read.
use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::slides::{
    api::SlidesApi,
    client::SlidesClient,
    read::{read, ReadOptions, ReadOutcome},
    target,
};
use anyhow::{Context, Result};
use clap::Parser;

/// Reads ordinary-slide elements and separately identified speaker notes.
#[derive(Parser)]
pub struct ReadCommand {
    /// Presentation id (the /d/<ID>/ segment of a Slides URL).
    pub presentation_id: String,
    /// Restrict to an ordinary slide object ID. Repeat for multiple slides.
    #[arg(long = "slide", value_name = "OBJECT_ID")]
    pub slides: Vec<String>,
    /// Output format. JSON/YAML/JSONL preserve original text.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}
impl ReadCommand {
    /// Builds options and delegates to the testable runner.
    pub async fn execute(self, drive: &DriveClient) -> Result<()> {
        let slides = SlidesClient::from_drive_client(drive)?;
        let opts = ReadOptions {
            presentation_id: self.presentation_id,
            slides: self.slides,
        };
        run_read(drive, &slides, &opts, &self.output).await
    }
}
async fn run_read(
    drive: &DriveClient,
    slides: &SlidesClient,
    opts: &ReadOptions,
    output: &OutputFormat,
) -> Result<()> {
    let outcome = match read(&SlidesApi::new(slides), opts).await {
        Ok(outcome) => outcome,
        Err(err) => {
            return Err(target::explain_failure(
                &FilesApi::new(drive),
                &opts.presentation_id,
                "read",
                err,
            )
            .await)
        }
    };
    if output_as(&outcome, output)? {
        return Ok(());
    }
    render_table(&outcome, &mut std::io::stdout().lock())
}
fn render_table(outcome: &ReadOutcome, out: &mut dyn std::io::Write) -> Result<()> {
    for e in &outcome.elements {
        let cell = e
            .cell
            .as_ref()
            .map(|c| format!("[{},{}]", c.row_index, c.column_index))
            .unwrap_or_default();
        writeln!(
            out,
            "{}\t{}\t{}\t{}{}\t{}\t{}",
            e.slide_index,
            sanitize_for_terminal(&e.slide_object_id),
            sanitize_for_terminal(&e.page_object_id),
            sanitize_for_terminal(&e.element_object_id),
            cell,
            e.kind,
            sanitize_for_terminal(&e.text).replace(['\n', '\r', '\t'], " ")
        )
        .context("Failed to write slides read")?;
    }
    Ok(())
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn table_sanitizes_text_and_ids() {
        let p = serde_json::from_value(serde_json::json!({"slides":[{"objectId":"s","pageElements":[{"objectId":"e\u{1b}","shape":{"text":{"textElements":[{"textRun":{"content":"hello\nworld\u{1b}"}}]}}}]}]})).unwrap();
        let outcome = ReadOutcome {
            presentation_id: "p".into(),
            title: None,
            revision_id: None,
            elements: crate::drive::slides::read::flatten(&p, &["s".into()], true),
        };
        let mut buf = Vec::new();
        render_table(&outcome, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert_eq!(text.lines().count(), 1);
    }

    use crate::drive::slides::client::test_support::mock_clients;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn serve(server: &MockServer, endpoint: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(response)
            .expect(1)
            .mount(server)
            .await;
    }
    fn api_error(code: u16, status: &str) -> ResponseTemplate {
        ResponseTemplate::new(code).set_body_json(
            serde_json::json!({"error":{"code":code,"status":status,"message":"nope"}}),
        )
    }
    fn opts(slides: &[&str]) -> ReadOptions {
        ReadOptions {
            presentation_id: "p1".into(),
            slides: slides.iter().map(|s| (*s).to_string()).collect(),
        }
    }
    fn deck() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "title": "Deck", "revisionId": "rev-1",
            "slides": [{"objectId":"s1","pageElements":[
                {"objectId":"shape","shape":{"text":{"textElements":[{"textRun":{"content":"hi"}}]}}},
                {"objectId":"table","table":{"tableRows":[{"tableCells":[{"text":{"textElements":[{"textRun":{"content":"cell"}}]}}]}]}}
            ]}]
        }))
    }

    #[tokio::test]
    async fn run_read_succeeds_in_every_output_format() {
        for output in [
            OutputFormat::Table,
            OutputFormat::Json,
            OutputFormat::Yaml,
            OutputFormat::Yamls,
            OutputFormat::Jsonl,
        ] {
            let server = MockServer::start().await;
            let (drive, slides) = mock_clients(&server).await;
            serve(&server, "/v1/presentations/p1", deck()).await;
            run_read(&drive, &slides, &opts(&["s1"]), &output)
                .await
                .unwrap();
        }
    }
    #[test]
    fn table_rows_carry_the_cell_coordinates_of_table_elements() {
        let p = serde_json::from_value(serde_json::json!({"slides":[{"objectId":"s","pageElements":[
            {"objectId":"t","table":{"tableRows":[{"tableCells":[{"text":{"textElements":[{"textRun":{"content":"x"}}]}},{"text":{"textElements":[{"textRun":{"content":"y"}}]}}]}]}}
        ]}]})).unwrap();
        let outcome = ReadOutcome {
            presentation_id: "p".into(),
            title: None,
            revision_id: None,
            elements: crate::drive::slides::read::flatten(&p, &["s".into()], true),
        };
        let mut buf = Vec::new();
        render_table(&outcome, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("t[0,0]"), "{text}");
        assert!(text.contains("t[0,1]"), "{text}");
    }
    #[tokio::test]
    async fn run_read_explains_a_target_that_is_not_a_presentation() {
        let server = MockServer::start().await;
        let (drive, slides) = mock_clients(&server).await;
        serve(&server, "/v1/presentations/p1", api_error(404, "NOT_FOUND")).await;
        serve(
            &server,
            "/drive/v3/files/p1",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"p1","name":"Notes","mimeType":crate::drive::types::GOOGLE_DOC_MIME_TYPE
            })),
        )
        .await;
        let err = run_read(&drive, &slides, &opts(&[]), &OutputFormat::Json)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a Google Slides presentation"), "{err}");
        assert!(err.contains("drive slides read"), "{err}");
    }
    #[tokio::test]
    async fn run_read_reports_an_unknown_slide_filter_unchanged_for_a_real_presentation() {
        let server = MockServer::start().await;
        let (drive, slides) = mock_clients(&server).await;
        serve(&server, "/v1/presentations/p1", deck()).await;
        serve(
            &server,
            "/drive/v3/files/p1",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"p1","name":"Deck","mimeType":crate::drive::types::GOOGLE_SLIDES_MIME_TYPE
            })),
        )
        .await;
        let err = run_read(&drive, &slides, &opts(&["nope"]), &OutputFormat::Json)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
        assert!(!err.contains("not a Google Slides presentation"), "{err}");
    }
    #[tokio::test]
    async fn run_read_keeps_the_slides_error_when_the_target_cannot_be_classified() {
        let server = MockServer::start().await;
        let (drive, slides) = mock_clients(&server).await;
        serve(
            &server,
            "/v1/presentations/p1",
            api_error(403, "PERMISSION_DENIED"),
        )
        .await;
        serve(&server, "/drive/v3/files/p1", ResponseTemplate::new(404)).await;
        let err = run_read(&drive, &slides, &opts(&[]), &OutputFormat::Json)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Slides API request failed"), "{err}");
    }
}
