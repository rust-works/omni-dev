//! CLI command for `omni-dev gmail render`.
//!
//! Renders one or more `.eml` files as human-readable Markdown (CLI-only; no
//! MCP equivalent; purely local, no client/credentials needed — #1513).
//! Deliberately takes file paths rather than a `Manifest`/archive-dir +
//! message id, so it works on any `.eml` file — piped a glob from a `gmail
//! sync` archive (`messages/<y>/<m>/<d>/*.eml`) or any other source — with
//! no dependency on this tool having synced the mailbox at all. The actual
//! rendering lives in [`crate::gmail::render::render_markdown`], shared with
//! `gmail read -o markdown` (`src/cli/gmail/read.rs`) so "MIME bytes ->
//! readable Markdown" logic is written once regardless of whether the bytes
//! came from disk or a live API fetch.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::cli::gmail::format::{output_as, OutputFormat};
use crate::gmail::render::render_markdown;

/// Renders one or more archived `.eml` files as Markdown.
#[derive(Parser)]
pub struct RenderCommand {
    /// One or more `.eml` file paths to render.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Writes one `.md` file per input (named after the input's stem) into
    /// this directory instead of printing Markdown to stdout.
    #[arg(long = "out-dir", value_name = "DIR")]
    pub out_dir: Option<PathBuf>,

    /// Report format. `Table` (default) prints each input's rendered
    /// Markdown directly to stdout — or, with `--out-dir`, a `Saved to:`
    /// line per file — so redirecting stdout to a file yields clean
    /// Markdown; json/yaml/yamls/jsonl instead emit one structured record
    /// per input.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl RenderCommand {
    /// Purely local and synchronous — no client, no `.await` anywhere in
    /// this command, mirroring `ExtractAttachmentsCommand::execute`.
    pub fn execute(self) -> Result<()> {
        run_render_command(&self.paths, self.out_dir.as_deref(), &self.output)
    }
}

/// One rendered (or failed) input, in input order. A plain, flat record
/// (rather than `extract_attachments.rs`'s separate `actions`/`errors`
/// lists) so `Vec<RenderedFile>` gets `JsonlSerialize` for free from the
/// shared blanket `impl<T: Serialize> JsonlSerialize for Vec<T>` — one line
/// per input under `-o jsonl`, which is more useful here than one line for
/// the whole batch given callers may render hundreds of archived messages
/// at once (see #1513's own motivating ~140-message report).
#[derive(Serialize)]
struct RenderedFile {
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    saved_to: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Renders every path and emits the report. Split from
/// [`RenderCommand::execute`] so tests can call it directly, mirroring
/// `extract_attachments.rs::run_extract_attachments_command`'s compute ->
/// render -> decide split (ADR-0064 Decision 4).
fn run_render_command(
    paths: &[PathBuf],
    out_dir: Option<&Path>,
    output: &OutputFormat,
) -> Result<()> {
    let rendered = render_paths(paths, out_dir)?;

    if !output_as(&rendered, output)? {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        render_report_text(&rendered, &mut handle)?;
    }

    let error_count = rendered.iter().filter(|file| file.error.is_some()).count();
    if error_count > 0 {
        anyhow::bail!(
            "{error_count} of {} file(s) failed to render; see errors above",
            rendered.len()
        );
    }
    Ok(())
}

/// Reads and renders each path in order, writing a `.md` file per input
/// under `out_dir` when given. A per-file read/write failure is recorded on
/// that file's [`RenderedFile::error`] rather than aborting the batch — the
/// same "don't let one bad file kill the whole run" posture
/// `gmail extract-attachments` takes.
fn render_paths(paths: &[PathBuf], out_dir: Option<&Path>) -> Result<Vec<RenderedFile>> {
    if let Some(dir) = out_dir {
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create output directory {}", dir.display()))?;
    }

    Ok(paths.iter().map(|path| render_one(path, out_dir)).collect())
}

fn render_one(path: &Path, out_dir: Option<&Path>) -> RenderedFile {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(err) => {
            return RenderedFile {
                path: path.to_path_buf(),
                saved_to: None,
                markdown: None,
                error: Some(err.to_string()),
            }
        }
    };
    let markdown = render_markdown(&raw);

    let Some(dir) = out_dir else {
        return RenderedFile {
            path: path.to_path_buf(),
            saved_to: None,
            markdown: Some(markdown),
            error: None,
        };
    };

    let out_path = dir.join(md_filename(path));
    match fs::write(&out_path, &markdown) {
        Ok(()) => RenderedFile {
            path: path.to_path_buf(),
            saved_to: Some(out_path),
            markdown: None,
            error: None,
        },
        Err(err) => RenderedFile {
            path: path.to_path_buf(),
            saved_to: None,
            markdown: None,
            error: Some(err.to_string()),
        },
    }
}

/// The `.md` output filename for an input path: its stem with a `.md`
/// extension (e.g. `messages/2024/01/02/abc123.eml` -> `abc123.md`),
/// mirroring `read.rs`'s single-file `--out-file` pattern extended to many
/// files at once. Two inputs sharing a stem (rare in practice, since
/// `gmail sync` names files after Gmail's own globally-unique-per-account
/// message ids) overwrite one another under `--out-dir` — not otherwise
/// guarded against, since that's a path the caller controls.
fn md_filename(path: &Path) -> PathBuf {
    let stem = path.file_stem().map_or_else(
        || "message".to_string(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    PathBuf::from(format!("{stem}.md"))
}

/// Renders the `Table` view. With `--out-dir` every entry has `saved_to` or
/// `error` set, so this prints one line per file; without it, every entry
/// has `markdown` or `error` set, so successfully-rendered files are
/// dumped directly (separated by a Markdown thematic break when there is
/// more than one), and failures are reported as trailing `Error:` lines —
/// deliberately no trailing summary line the way `extract_attachments.rs`
/// always prints one, since that would corrupt an otherwise-clean
/// `gmail render *.eml > combined.md` redirect in the all-success case.
fn render_report_text(rendered: &[RenderedFile], out: &mut dyn Write) -> Result<()> {
    let mut wrote_markdown = false;
    for file in rendered {
        if let Some(error) = &file.error {
            writeln!(out, "Error: {}: {error}", file.path.display())
                .context("Failed to write render report")?;
            continue;
        }
        if let Some(saved_to) = &file.saved_to {
            writeln!(out, "Saved to: {}", saved_to.display())
                .context("Failed to write render report")?;
            continue;
        }
        if let Some(markdown) = &file.markdown {
            if wrote_markdown {
                writeln!(out, "\n---\n").context("Failed to write render report")?;
            }
            write!(out, "{markdown}").context("Failed to write render report")?;
            wrote_markdown = true;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn write_eml(dir: &Path, name: &str, raw: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, raw).unwrap();
        path
    }

    const PLAIN_MESSAGE: &str = "Subject: Hello\r\nFrom: a@example.com\r\n\r\nHi there.";

    #[test]
    fn run_render_command_prints_markdown_to_stdout_for_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);

        run_render_command(&[path], None, &OutputFormat::Table).unwrap();
    }

    #[test]
    fn render_paths_renders_markdown_without_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);

        let rendered = render_paths(std::slice::from_ref(&path), None).unwrap();
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].path, path);
        assert!(rendered[0].saved_to.is_none());
        assert!(rendered[0]
            .markdown
            .as_deref()
            .unwrap()
            .contains("Hi there."));
        assert!(rendered[0].error.is_none());
    }

    #[test]
    fn render_paths_writes_md_file_named_after_input_stem_with_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "abc123.eml", PLAIN_MESSAGE);
        let out_dir = dir.path().join("out");

        let rendered = render_paths(&[path], Some(&out_dir)).unwrap();
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].markdown.is_none());
        let saved_to = rendered[0].saved_to.clone().unwrap();
        assert_eq!(saved_to, out_dir.join("abc123.md"));
        assert!(fs::read_to_string(&saved_to).unwrap().contains("Hi there."));
    }

    #[test]
    fn render_paths_creates_out_dir_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);
        let out_dir = dir.path().join("nested").join("out");
        assert!(!out_dir.exists());

        render_paths(&[path], Some(&out_dir)).unwrap();
        assert!(out_dir.is_dir());
    }

    #[test]
    fn render_paths_records_error_for_missing_file_without_aborting_batch() {
        let dir = tempfile::tempdir().unwrap();
        let good = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);
        let missing = dir.path().join("does-not-exist.eml");

        let rendered = render_paths(&[good, missing.clone()], None).unwrap();
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].markdown.is_some());
        assert!(rendered[0].error.is_none());
        assert_eq!(rendered[1].path, missing);
        assert!(rendered[1].error.is_some());
        assert!(rendered[1].markdown.is_none());
    }

    #[test]
    fn run_render_command_surfaces_a_non_zero_exit_on_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.eml");

        let err = run_render_command(&[missing], None, &OutputFormat::Table).unwrap_err();
        assert!(err.to_string().contains("1 of 1 file(s) failed to render"));
    }

    #[test]
    fn run_render_command_still_renders_successful_files_when_others_fail() {
        let dir = tempfile::tempdir().unwrap();
        let good = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);
        let missing = dir.path().join("does-not-exist.eml");

        let rendered = render_paths(&[good, missing], None).unwrap();
        assert_eq!(rendered.iter().filter(|f| f.error.is_none()).count(), 1);
    }

    #[test]
    fn run_render_command_renders_json_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);

        run_render_command(&[path], None, &OutputFormat::Json).unwrap();
    }

    #[test]
    fn md_filename_uses_input_stem() {
        assert_eq!(
            md_filename(Path::new("messages/2024/01/02/abc123.eml")),
            PathBuf::from("abc123.md")
        );
    }

    #[test]
    fn md_filename_falls_back_to_message_when_input_has_no_file_stem() {
        assert_eq!(md_filename(Path::new("..")), PathBuf::from("message.md"));
    }

    #[test]
    fn render_paths_records_error_when_writing_the_output_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);
        let out_dir = dir.path().join("out");
        fs::create_dir_all(&out_dir).unwrap();
        // A directory sitting where the rendered `.md` file would be
        // written makes `fs::write` fail with "Is a directory".
        fs::create_dir_all(out_dir.join("m1.md")).unwrap();

        let rendered = render_paths(&[path], Some(&out_dir)).unwrap();
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].saved_to.is_none());
        assert!(rendered[0].markdown.is_none());
        assert!(rendered[0].error.is_some());
    }

    #[test]
    fn render_report_text_joins_multiple_files_with_thematic_break() {
        let rendered = vec![
            RenderedFile {
                path: PathBuf::from("a.eml"),
                saved_to: None,
                markdown: Some("First\n".to_string()),
                error: None,
            },
            RenderedFile {
                path: PathBuf::from("b.eml"),
                saved_to: None,
                markdown: Some("Second\n".to_string()),
                error: None,
            },
        ];
        let mut buf = Vec::new();
        render_report_text(&rendered, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("First\n\n---\n\nSecond\n"));
    }

    #[test]
    fn render_report_text_reports_errors_as_error_lines() {
        let rendered = vec![RenderedFile {
            path: PathBuf::from("a.eml"),
            saved_to: None,
            markdown: None,
            error: Some("No such file or directory".to_string()),
        }];
        let mut buf = Vec::new();
        render_report_text(&rendered, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Error: a.eml: No such file or directory"));
    }

    #[test]
    fn render_report_text_reports_saved_to_lines() {
        let rendered = vec![RenderedFile {
            path: PathBuf::from("a.eml"),
            saved_to: Some(PathBuf::from("out/a.md")),
            markdown: None,
            error: None,
        }];
        let mut buf = Vec::new();
        render_report_text(&rendered, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text, "Saved to: out/a.md\n");
    }

    #[test]
    fn render_report_text_writes_nothing_for_an_entry_with_no_output() {
        let rendered = vec![RenderedFile {
            path: PathBuf::from("a.eml"),
            saved_to: None,
            markdown: None,
            error: None,
        }];
        let mut buf = Vec::new();
        render_report_text(&rendered, &mut buf).unwrap();
        assert!(buf.is_empty());
    }
}
