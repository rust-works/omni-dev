//! CLI outline of a presentation.
use crate::cli::drive::format::{
    output_as, sanitize_for_terminal, write_scalar_jsonl, JsonlSerialize, OutputFormat,
};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::slides::{api::SlidesApi, client::SlidesClient, target, types::Presentation};
use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

/// Shows the presentation identity and its ordinary-slide outline.
#[derive(Parser)]
pub struct InfoCommand {
    /// Presentation id (the /d/<ID>/ segment of a Slides URL).
    pub presentation_id: String,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

#[derive(Debug, Serialize)]
struct InfoOutcome {
    presentation_id: String,
    title: Option<String>,
    revision_id: Option<String>,
    page_size: Option<serde_json::Value>,
    slides: Vec<SlideOutline>,
}
#[derive(Debug, Serialize)]
struct SlideOutline {
    slide_index: usize,
    slide_object_id: String,
    page_elements: usize,
    notes_page_id: Option<String>,
}
impl InfoOutcome {
    fn of(id: &str, p: &Presentation) -> Self {
        Self {
            presentation_id: id.into(),
            title: p.title.clone(),
            revision_id: p.revision_id.clone(),
            page_size: p.page_size.clone(),
            slides: p
                .slides
                .iter()
                .enumerate()
                .map(|(index, slide)| SlideOutline {
                    slide_index: index + 1,
                    slide_object_id: slide.object_id.clone(),
                    page_elements: slide.page_elements.len(),
                    notes_page_id: slide
                        .slide_properties
                        .as_ref()
                        .and_then(|s| s.notes_page.as_ref())
                        .map(|p| p.object_id.clone()),
                })
                .collect(),
        }
    }
}
impl JsonlSerialize for InfoOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}
impl InfoCommand {
    /// Derives the Slides host client and delegates to the testable runner.
    pub async fn execute(self, drive: &DriveClient) -> Result<()> {
        let slides = SlidesClient::from_drive_client(drive)?;
        run_info(drive, &slides, &self.presentation_id, &self.output).await
    }
}
async fn run_info(
    drive: &DriveClient,
    slides: &SlidesClient,
    id: &str,
    output: &OutputFormat,
) -> Result<()> {
    let p = match SlidesApi::new(slides).get_presentation(id).await {
        Ok(p) => p,
        Err(err) => {
            return Err(target::explain_failure(&FilesApi::new(drive), id, "info", err).await)
        }
    };
    let outcome = InfoOutcome::of(id, &p);
    if output_as(&outcome, output)? {
        return Ok(());
    }
    render_table(&outcome, &mut std::io::stdout().lock())
}
fn render_table(outcome: &InfoOutcome, out: &mut dyn std::io::Write) -> Result<()> {
    writeln!(
        out,
        "Id: {}\nTitle: {}\nRevision: {}\nSlides: {}",
        sanitize_for_terminal(&outcome.presentation_id),
        sanitize_for_terminal(outcome.title.as_deref().unwrap_or("")),
        sanitize_for_terminal(
            outcome
                .revision_id
                .as_deref()
                .unwrap_or("(none — edit access required for writes)")
        ),
        outcome.slides.len()
    )
    .context("Failed to write slides info")?;
    for slide in &outcome.slides {
        writeln!(
            out,
            "{}\t{}\t{} element(s)",
            slide.slide_index,
            sanitize_for_terminal(&slide.slide_object_id),
            slide.page_elements
        )
        .context("Failed to write slides info")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn outline_preserves_order_and_table_sanitizes_titles() {
        let p: Presentation = serde_json::from_value(serde_json::json!({"title":"Deck\u{1b}[31m", "slides":[{"objectId":"second"},{"objectId":"first"}]})).unwrap();
        let outcome = InfoOutcome::of("p", &p);
        assert_eq!(outcome.slides[0].slide_object_id, "second");
        let mut buf = Vec::new();
        render_table(&outcome, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("edit access required"));
    }
}
