//! CLI command for `omni-dev gmail render`.
//!
//! Renders one or more `.eml` files as human-readable Markdown (CLI-only; no
//! MCP equivalent; purely local, no client/credentials needed — #1513). By
//! default it takes bare file paths rather than a `Manifest`/archive-dir +
//! message id, so it works on any `.eml` file — piped a glob from a `gmail
//! sync` archive (`messages/<y>/<m>/<d>/*.eml`) or any other source — with
//! no dependency on this tool having synced the mailbox at all.
//! `--archive-dir PATH --all` is the opt-in alternative for the common
//! "render everything I've synced" case (#1515): it reads `manifest.jsonl`
//! and resolves the same paths a caller's own glob would have, but skips
//! soft-deleted messages and (with `--out-dir`) messages already rendered by
//! a prior run, mirroring `gmail extract-attachments --archive-dir`'s
//! presence-on-disk idempotence (#1510). The actual rendering lives in
//! [`crate::gmail::render::render_markdown`], shared with `gmail read -o
//! markdown` (`src/cli/gmail/read.rs`) so "MIME bytes -> readable Markdown"
//! logic is written once regardless of whether the bytes came from disk or a
//! live API fetch.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{ArgGroup, Parser};
use serde::Serialize;

use crate::cli::gmail::format::{output_as, OutputFormat};
use crate::cli::gmail::sync::engine::manifest_path;
use crate::cli::gmail::sync::manifest::Manifest;
use crate::gmail::render::render_markdown;

/// Renders one or more archived `.eml` files as Markdown.
#[derive(Parser)]
#[command(group(
    ArgGroup::new("render_source")
        .required(true)
        .args(["paths", "archive_dir"]),
))]
pub struct RenderCommand {
    /// One or more `.eml` file paths to render. Mutually exclusive with
    /// `--archive-dir`.
    #[arg(value_name = "PATH", group = "render_source")]
    pub paths: Vec<PathBuf>,

    /// Archive directory previously populated by `gmail sync`/`sync-all`.
    /// Renders every non-deleted message from its `manifest.jsonl`.
    /// Mutually exclusive with positional `PATH` arguments; requires
    /// `--all`.
    #[arg(long, value_name = "PATH", group = "render_source", requires = "all")]
    pub archive_dir: Option<PathBuf>,

    /// Confirms whole-archive rendering with `--archive-dir`. Requires
    /// `--archive-dir`. A separate flag (rather than `--archive-dir` alone
    /// implying it) reserves room for a future non-`--all` selector, e.g.
    /// by message id.
    #[arg(long, requires = "archive_dir")]
    pub all: bool,

    /// Writes one `.md` file per input (named after the input's stem) into
    /// this directory instead of printing Markdown to stdout. With
    /// `--archive-dir --all`, also makes a message already rendered by a
    /// prior run into this directory skipped on this run.
    #[arg(long = "out-dir", value_name = "DIR")]
    pub out_dir: Option<PathBuf>,

    /// Report format. `Table` (default) prints each input's rendered
    /// Markdown directly to stdout — or, with `--out-dir`, a `Saved to:`
    /// line per file — so redirecting stdout to a file yields clean
    /// Markdown; json/yaml/yamls/jsonl instead emit one structured record
    /// per input.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Collapses `>`-quoted reply history nested more than one level deep
    /// into a one-line `*(N quoted lines omitted)*` marker (#1514). Off by
    /// default: verbatim rendering is fully information-preserving, and the
    /// full text is one re-render away without this flag.
    #[arg(long)]
    pub fold_quotes: bool,
}

impl RenderCommand {
    /// Purely local and synchronous — no client, no `.await` anywhere in
    /// this command, mirroring `ExtractAttachmentsCommand::execute`.
    pub fn execute(self) -> Result<()> {
        let paths = match &self.archive_dir {
            Some(archive_dir) => resolve_archive_paths(archive_dir, self.out_dir.as_deref())?,
            None => self.paths,
        };
        run_render_command(
            &paths,
            self.out_dir.as_deref(),
            &self.output,
            self.fold_quotes,
        )
    }
}

/// Resolves every non-deleted manifest record under `archive_dir` to its
/// archived `.eml` path, for `--archive-dir --all`. Skips a record whose
/// rendered `.md` already exists under `out_dir` — the same
/// presence-on-disk idempotence `gmail extract-attachments` relies on for
/// `attachments/` dirs (#1510), silently, matching its "already extracted"
/// skip — so a large archive can be re-rendered incrementally without
/// redoing work. No skip is applied when `out_dir` is `None`: stdout has
/// nothing durable to check a re-run against. Reuses [`md_filename`] so this
/// skip check's notion of "already rendered" can never drift from where
/// [`render_one`] actually writes the file.
fn resolve_archive_paths(archive_dir: &Path, out_dir: Option<&Path>) -> Result<Vec<PathBuf>> {
    let manifest = Manifest::load(&manifest_path(archive_dir))?;
    Ok(manifest
        .records_not_deleted()
        .map(|record| archive_dir.join(&record.path))
        .filter(|eml_path| match out_dir {
            Some(dir) => !dir.join(md_filename(eml_path)).exists(),
            None => true,
        })
        .collect())
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
    fold_quotes: bool,
) -> Result<()> {
    let rendered = render_paths(paths, out_dir, fold_quotes)?;

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
fn render_paths(
    paths: &[PathBuf],
    out_dir: Option<&Path>,
    fold_quotes: bool,
) -> Result<Vec<RenderedFile>> {
    if let Some(dir) = out_dir {
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create output directory {}", dir.display()))?;
    }

    Ok(paths
        .iter()
        .map(|path| render_one(path, out_dir, fold_quotes))
        .collect())
}

fn render_one(path: &Path, out_dir: Option<&Path>, fold_quotes: bool) -> RenderedFile {
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
    let markdown = render_markdown(&raw, fold_quotes);

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

        run_render_command(&[path], None, &OutputFormat::Table, false).unwrap();
    }

    #[test]
    fn render_paths_renders_markdown_without_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);

        let rendered = render_paths(std::slice::from_ref(&path), None, false).unwrap();
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

        let rendered = render_paths(&[path], Some(&out_dir), false).unwrap();
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

        render_paths(&[path], Some(&out_dir), false).unwrap();
        assert!(out_dir.is_dir());
    }

    #[test]
    fn render_paths_records_error_for_missing_file_without_aborting_batch() {
        let dir = tempfile::tempdir().unwrap();
        let good = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);
        let missing = dir.path().join("does-not-exist.eml");

        let rendered = render_paths(&[good, missing.clone()], None, false).unwrap();
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

        let err = run_render_command(&[missing], None, &OutputFormat::Table, false).unwrap_err();
        assert!(err.to_string().contains("1 of 1 file(s) failed to render"));
    }

    #[test]
    fn run_render_command_still_renders_successful_files_when_others_fail() {
        let dir = tempfile::tempdir().unwrap();
        let good = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);
        let missing = dir.path().join("does-not-exist.eml");

        let rendered = render_paths(&[good, missing], None, false).unwrap();
        assert_eq!(rendered.iter().filter(|f| f.error.is_none()).count(), 1);
    }

    #[test]
    fn run_render_command_renders_json_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(dir.path(), "m1.eml", PLAIN_MESSAGE);

        run_render_command(&[path], None, &OutputFormat::Json, false).unwrap();
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

        let rendered = render_paths(&[path], Some(&out_dir), false).unwrap();
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].saved_to.is_none());
        assert!(rendered[0].markdown.is_none());
        assert!(rendered[0].error.is_some());
    }

    #[test]
    fn render_paths_folds_nested_quotes_when_fold_quotes_is_true() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "Subject: Hi\r\nFrom: a@example.com\r\nContent-Type: text/plain\r\n\r\nNew reply.\r\n\r\n> Alice wrote:\r\n>> Original message.\r\n";
        let path = write_eml(dir.path(), "m1.eml", raw);

        let rendered = render_paths(&[path], None, true).unwrap();
        let markdown = rendered[0].markdown.as_deref().unwrap();
        assert!(markdown.contains("New reply."));
        assert!(markdown.contains("omitted"));
        assert!(!markdown.contains("Original message."));
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

    // ── --archive-dir --all ─────────────────────────────────────────

    use crate::cli::gmail::sync::manifest::ManifestRecord;

    /// Writes `.eml` bytes under `archive_dir` at `eml_relative` and
    /// upserts a matching manifest record, creating/updating
    /// `manifest.jsonl` as needed — mirrors
    /// `extract_attachments.rs::write_manifest_with_attachment`.
    fn write_manifest_record(
        archive_dir: &Path,
        id: &str,
        eml_relative: &str,
        raw: &str,
        deleted: bool,
    ) {
        let eml_path = archive_dir.join(eml_relative);
        fs::create_dir_all(eml_path.parent().unwrap()).unwrap();
        fs::write(&eml_path, raw).unwrap();

        let manifest_file = manifest_path(archive_dir);
        let mut manifest = if manifest_file.exists() {
            Manifest::load(&manifest_file).unwrap()
        } else {
            Manifest::default()
        };
        manifest.upsert(ManifestRecord {
            id: id.to_string(),
            thread_id: None,
            label_ids: Vec::new(),
            internal_date: None,
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: None,
            in_reply_to: None,
            references: None,
            attachment_count: 0,
            attachment_filenames: Vec::new(),
            path: PathBuf::from(eml_relative),
            size: raw.len() as u64,
            history_id: None,
            deleted_at: deleted.then(chrono::Utc::now),
        });
        manifest.save(&manifest_file).unwrap();
    }

    #[test]
    fn resolve_archive_paths_resolves_non_deleted_records() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_record(&archive_dir, "m1", "messages/m1.eml", PLAIN_MESSAGE, false);

        let resolved = resolve_archive_paths(&archive_dir, None).unwrap();
        assert_eq!(resolved, vec![archive_dir.join("messages/m1.eml")]);
    }

    #[test]
    fn resolve_archive_paths_skips_soft_deleted_records() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_record(&archive_dir, "m1", "messages/m1.eml", PLAIN_MESSAGE, false);
        write_manifest_record(&archive_dir, "m2", "messages/m2.eml", PLAIN_MESSAGE, true);

        let resolved = resolve_archive_paths(&archive_dir, None).unwrap();
        assert_eq!(resolved, vec![archive_dir.join("messages/m1.eml")]);
    }

    #[test]
    fn resolve_archive_paths_skips_already_rendered_files_under_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_record(&archive_dir, "m1", "messages/m1.eml", PLAIN_MESSAGE, false);
        let out_dir = dir.path().join("out");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("m1.md"), "already rendered").unwrap();

        let resolved = resolve_archive_paths(&archive_dir, Some(&out_dir)).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_archive_paths_applies_no_skip_without_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_record(&archive_dir, "m1", "messages/m1.eml", PLAIN_MESSAGE, false);

        // No out_dir given, so there is nothing durable to skip against,
        // even though a same-named file happens to exist elsewhere.
        let resolved = resolve_archive_paths(&archive_dir, None).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn resolve_archive_paths_empty_for_missing_archive_dir() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("does-not-exist");

        let resolved = resolve_archive_paths(&archive_dir, None).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn execute_archive_dir_all_renders_then_skips_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_record(&archive_dir, "m1", "messages/m1.eml", PLAIN_MESSAGE, false);
        let out_dir = dir.path().join("out");

        let cmd = RenderCommand {
            paths: Vec::new(),
            archive_dir: Some(archive_dir.clone()),
            all: true,
            out_dir: Some(out_dir.clone()),
            output: OutputFormat::Table,
            fold_quotes: false,
        };
        cmd.execute().unwrap();
        let md_path = out_dir.join("m1.md");
        assert!(fs::read_to_string(&md_path).unwrap().contains("Hi there."));
        let first_write = fs::metadata(&md_path).unwrap().modified().unwrap();

        // Re-running must be a clean no-op: the file already exists under
        // `out_dir`, so `resolve_archive_paths` skips it and nothing is
        // rewritten.
        let cmd = RenderCommand {
            paths: Vec::new(),
            archive_dir: Some(archive_dir),
            all: true,
            out_dir: Some(out_dir),
            output: OutputFormat::Table,
            fold_quotes: false,
        };
        cmd.execute().unwrap();
        assert_eq!(
            fs::metadata(&md_path).unwrap().modified().unwrap(),
            first_write
        );
    }

    #[test]
    fn archive_dir_without_all_fails_to_parse() {
        assert!(
            RenderCommand::try_parse_from(["render", "--archive-dir", "/tmp/archive"]).is_err()
        );
    }

    #[test]
    fn all_without_archive_dir_fails_to_parse() {
        assert!(RenderCommand::try_parse_from(["render", "--all"]).is_err());
    }

    #[test]
    fn paths_combined_with_archive_dir_fails_to_parse() {
        assert!(RenderCommand::try_parse_from([
            "render",
            "m1.eml",
            "--archive-dir",
            "/tmp/archive",
            "--all",
        ])
        .is_err());
    }

    #[test]
    fn neither_paths_nor_archive_dir_fails_to_parse() {
        assert!(RenderCommand::try_parse_from(["render"]).is_err());
    }

    #[test]
    fn bare_paths_still_parse() {
        let cmd = RenderCommand::try_parse_from(["render", "m1.eml", "m2.eml"]).unwrap();
        assert_eq!(
            cmd.paths,
            vec![PathBuf::from("m1.eml"), PathBuf::from("m2.eml")]
        );
        assert!(cmd.archive_dir.is_none());
        assert!(!cmd.all);
    }
}
