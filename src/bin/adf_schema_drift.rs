//! `adf-schema-drift` — fetch the latest `@atlaskit/adf-schema` upstream and
//! diff it against the locally-encoded snapshot in
//! `src/atlassian/adf_schema/mod.rs`.
//!
//! Used by `.github/workflows/adf-schema-drift.yml` on a weekly schedule.
//! Writes `drift-report.md` and/or `drift-report.json` to `--output-dir` and
//! emits `drift=<bool>` and `version_changed=<bool>` to stdout (and to
//! `$GITHUB_OUTPUT` if set), so the workflow can decide whether to open or
//! update a tracking issue.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use omni_dev::atlassian::adf_schema::drift::{fetch_latest_drift_report, DriftReport};

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Format {
    Markdown,
    Json,
    Both,
}

#[derive(Debug, Parser)]
#[command(
    name = "adf-schema-drift",
    about = "Detect drift between the local ADF schema snapshot and upstream @atlaskit/adf-schema"
)]
struct Cli {
    /// Output format(s) to write to `--output-dir`.
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,

    /// Directory to write report files into. Created if it does not exist.
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let report = fetch_latest_drift_report().await?;
    process_report(
        &report,
        cli.format,
        &cli.output_dir,
        std::env::var("GITHUB_OUTPUT").ok().as_deref(),
        &mut std::io::stdout().lock(),
    )
}

/// Write requested report files, append GitHub-Actions step outputs, and
/// echo the same signals to `stdout`. Factored out of `main` so it can be
/// unit-tested without spinning up a real HTTP fetch.
fn process_report(
    report: &DriftReport,
    format: Format,
    output_dir: &Path,
    github_output: Option<&str>,
    stdout: &mut dyn Write,
) -> Result<()> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("creating output directory {}", output_dir.display()))?;

    let want_md = matches!(format, Format::Markdown | Format::Both);
    let want_json = matches!(format, Format::Json | Format::Both);

    if want_md {
        let path = output_dir.join("drift-report.md");
        fs::write(&path, report.render_markdown())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if want_json {
        let path = output_dir.join("drift-report.json");
        let body = serde_json::to_string_pretty(&report.render_json())
            .context("serialising drift report to JSON")?;
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }

    let drift = report.has_any_drift();
    writeln!(stdout, "drift={drift}").context("writing drift= to stdout")?;
    writeln!(stdout, "version_changed={}", report.version_changed)
        .context("writing version_changed= to stdout")?;

    if let Some(path) = github_output {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .with_context(|| format!("opening $GITHUB_OUTPUT at {path}"))?;
        writeln!(f, "drift={drift}").context("appending drift= to $GITHUB_OUTPUT")?;
        writeln!(f, "version_changed={}", report.version_changed)
            .context("appending version_changed= to $GITHUB_OUTPUT")?;
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::{BTreeMap, BTreeSet};

    fn fixture_report(version_changed: bool, with_content_drift: bool) -> DriftReport {
        let mut per_parent = BTreeMap::new();
        if with_content_drift {
            per_parent.insert(
                "blockquote".to_string(),
                omni_dev::atlassian::adf_schema::drift::ParentDrift {
                    added_children: std::iter::once("madeUp").map(String::from).collect(),
                    removed_children: BTreeSet::new(),
                },
            );
        }
        DriftReport {
            upstream_version: "99.0.0".to_string(),
            upstream_tarball_sha256: "sha".to_string(),
            local_version: "52.9.5-2026-05-10".to_string(),
            local_tarball_sha256: "localsha".to_string(),
            version_changed,
            per_parent,
            added_parents: BTreeSet::new(),
            removed_parents: BTreeSet::new(),
        }
    }

    #[test]
    fn cli_defaults_match_documentation() {
        let cli = Cli::parse_from(["adf-schema-drift"]);
        assert_eq!(cli.format, Format::Markdown);
        assert_eq!(cli.output_dir, PathBuf::from("."));
    }

    #[test]
    fn cli_accepts_explicit_format_and_output_dir() {
        let cli = Cli::parse_from([
            "adf-schema-drift",
            "--format",
            "both",
            "--output-dir",
            "/tmp/example",
        ]);
        assert_eq!(cli.format, Format::Both);
        assert_eq!(cli.output_dir, PathBuf::from("/tmp/example"));
    }

    #[test]
    fn process_report_writes_only_markdown_when_format_is_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let report = fixture_report(false, false);
        let mut buf: Vec<u8> = Vec::new();
        process_report(&report, Format::Markdown, dir.path(), None, &mut buf).unwrap();

        assert!(dir.path().join("drift-report.md").exists());
        assert!(!dir.path().join("drift-report.json").exists());
        let stdout = String::from_utf8(buf).unwrap();
        assert!(stdout.contains("drift=false"));
        assert!(stdout.contains("version_changed=false"));
    }

    #[test]
    fn process_report_writes_only_json_when_format_is_json() {
        let dir = tempfile::tempdir().unwrap();
        let report = fixture_report(true, false);
        let mut buf: Vec<u8> = Vec::new();
        process_report(&report, Format::Json, dir.path(), None, &mut buf).unwrap();

        assert!(!dir.path().join("drift-report.md").exists());
        assert!(dir.path().join("drift-report.json").exists());
        let stdout = String::from_utf8(buf).unwrap();
        assert!(stdout.contains("drift=true"));
        assert!(stdout.contains("version_changed=true"));
    }

    #[test]
    fn process_report_writes_both_when_format_is_both() {
        let dir = tempfile::tempdir().unwrap();
        let report = fixture_report(false, true);
        let mut buf: Vec<u8> = Vec::new();
        process_report(&report, Format::Both, dir.path(), None, &mut buf).unwrap();

        assert!(dir.path().join("drift-report.md").exists());
        assert!(dir.path().join("drift-report.json").exists());
        let stdout = String::from_utf8(buf).unwrap();
        // Content drift counts as drift even if version is unchanged.
        assert!(stdout.contains("drift=true"));
        assert!(stdout.contains("version_changed=false"));
    }

    #[test]
    fn process_report_appends_to_github_output_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        let github_output_path = dir.path().join("github_output.txt");
        // Seed with prior content to verify *append* (not overwrite).
        std::fs::write(&github_output_path, "existing-line\n").unwrap();

        let report = fixture_report(true, true);
        let mut buf: Vec<u8> = Vec::new();
        process_report(
            &report,
            Format::Markdown,
            dir.path(),
            Some(github_output_path.to_str().unwrap()),
            &mut buf,
        )
        .unwrap();

        let contents = std::fs::read_to_string(&github_output_path).unwrap();
        assert!(contents.starts_with("existing-line\n"));
        assert!(contents.contains("drift=true"));
        assert!(contents.contains("version_changed=true"));
    }

    #[test]
    fn process_report_creates_output_dir_if_missing() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("does/not/exist/yet");
        let report = fixture_report(false, false);
        let mut buf: Vec<u8> = Vec::new();
        process_report(&report, Format::Markdown, &nested, None, &mut buf).unwrap();
        assert!(nested.join("drift-report.md").exists());
    }

    #[test]
    fn process_report_errors_when_output_dir_path_is_blocked_by_file() {
        // Place a regular file at the path component the binary needs to
        // create as a directory; `create_dir_all` then fails.
        let parent = tempfile::tempdir().unwrap();
        let blocker = parent.path().join("blocker");
        std::fs::write(&blocker, b"not-a-directory").unwrap();
        let blocked = blocker.join("would-be-output-dir");

        let report = fixture_report(false, false);
        let mut buf: Vec<u8> = Vec::new();
        let err = process_report(&report, Format::Markdown, &blocked, None, &mut buf).unwrap_err();
        assert!(
            err.to_string().contains("creating output directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn process_report_errors_when_markdown_write_fails() {
        // Pre-create `drift-report.md` as a directory so the file write fails.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("drift-report.md")).unwrap();

        let report = fixture_report(false, false);
        let mut buf: Vec<u8> = Vec::new();
        let err =
            process_report(&report, Format::Markdown, dir.path(), None, &mut buf).unwrap_err();
        assert!(
            err.to_string().contains("drift-report.md"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn process_report_errors_when_json_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("drift-report.json")).unwrap();

        let report = fixture_report(false, false);
        let mut buf: Vec<u8> = Vec::new();
        let err = process_report(&report, Format::Json, dir.path(), None, &mut buf).unwrap_err();
        assert!(
            err.to_string().contains("drift-report.json"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn process_report_errors_when_github_output_path_is_unopenable() {
        let parent = tempfile::tempdir().unwrap();
        // A file whose parent is an existing regular file → can't open as
        // a writable file (parent isn't a directory).
        let blocker = parent.path().join("blocker");
        std::fs::write(&blocker, b"not-a-directory").unwrap();
        let bad_github_output = blocker.join("github_output.txt");

        let report = fixture_report(false, false);
        let mut buf: Vec<u8> = Vec::new();
        let err = process_report(
            &report,
            Format::Markdown,
            parent.path(),
            Some(bad_github_output.to_str().unwrap()),
            &mut buf,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("$GITHUB_OUTPUT"),
            "unexpected error: {err}"
        );
    }

    /// `Write` impl that always errors — covers stdout-write error contexts.
    struct AlwaysErroringWriter;
    impl Write for AlwaysErroringWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("boom"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn always_erroring_writer_methods_behave_as_documented() {
        // Sanity-check the test helper itself: `write` errors, `flush` is a
        // no-op `Ok`. Without this, `flush` would be uncovered because
        // `process_report` uses `writeln!` (which calls `write`/`write_all`)
        // and never explicitly flushes.
        let mut w = AlwaysErroringWriter;
        assert!(w.write(b"data").is_err());
        assert!(w.flush().is_ok());
    }

    #[test]
    fn process_report_errors_when_stdout_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let report = fixture_report(false, false);
        let mut writer = AlwaysErroringWriter;
        let err =
            process_report(&report, Format::Markdown, dir.path(), None, &mut writer).unwrap_err();
        assert!(
            err.to_string().contains("stdout"),
            "unexpected error: {err}"
        );
    }
}
