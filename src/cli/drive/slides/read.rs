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
}
