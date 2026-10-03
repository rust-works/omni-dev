//! `omni-dev coverage merge` — one report from the shards of a sharded run.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::Parser;
use git2::Repository;

use super::diff::{anchor, read_report, ReportFormat};
use crate::coverage::merge::{prefix_mismatch, require_executable_lines};
use crate::coverage::render::pct;
use crate::coverage::{lcov, CoverageReport};

/// Merges the per-shard coverage reports of a sharded run into one lcov file.
///
/// The result is what `coverage diff` computes from the same shards given as
/// repeated `--report`: the union of the files and of each file's executable
/// lines, taking the larger hit count for a line present in several. It is a
/// single file, which is what `--baseline-report` and other lcov consumers (a
/// codecov upload) need. Joining lcov files with `cat` is no substitute:
/// `cargo llvm-cov` writes no newline after its last `end_of_record`, so the
/// join glues it onto the next shard's first line.
///
/// Each input may be lcov, llvm-cov JSON or Cobertura, detected per file. An
/// input that is missing, unparseable or has no executable lines fails the run,
/// naming it, because a shard that silently produced nothing would only make
/// the total look slightly worse. An input whose absolute paths all fall
/// outside the strip prefix draws a warning.
///
/// The output is lcov with files and lines sorted and a trailing newline, so it
/// is byte-identical whatever order the inputs are given in. Only line coverage
/// is kept: function (`FN*`) and branch (`BRDA`) records are dropped. Paths are
/// written repo-relative, by stripping the repository working directory (or
/// `--strip-prefix`) from each.
#[derive(Parser)]
pub struct MergeCommand {
    /// Coverage reports to merge (lcov / llvm-cov-json / cobertura), one per shard.
    #[arg(value_name = "REPORT", required = true)]
    pub report: Vec<PathBuf>,

    /// Format of every report (auto-detected per file by default).
    #[arg(long, value_enum, default_value_t = ReportFormat::Auto)]
    pub report_format: ReportFormat,

    /// File to write the merged lcov report to.
    ///
    /// Unlike `coverage diff -o`, which selects an output *format*, this is a
    /// path. The file is replaced atomically: a merge that fails leaves an
    /// existing file untouched, and a path that is also an input is allowed.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: PathBuf,

    /// Prefix stripped from report file paths to make them repo-relative
    /// (default: the working directory of the repository, if there is one).
    ///
    /// Outside a repository, and without this flag, paths are written as the
    /// reports have them.
    #[arg(long, value_name = "PATH")]
    pub strip_prefix: Option<PathBuf>,
}

/// What a merge produced, separated from printing so tests can inspect it.
#[derive(Debug)]
pub struct MergeOutcome {
    /// The file the merged report was written to.
    pub output: PathBuf,
    /// Number of input reports merged.
    pub inputs: usize,
    /// Number of files with executable lines in the merged report.
    pub files: usize,
    /// Executable lines in the merged report.
    pub total_lines: u64,
    /// Executable lines in the merged report hit at least once.
    pub covered_lines: u64,
    /// Line coverage of the merged report, as a percentage.
    pub percent: Option<f64>,
    /// Non-fatal problems found while loading the reports (for example a shard
    /// measured under a different workspace root). [`MergeCommand::execute`]
    /// prints them to stderr.
    pub warnings: Vec<String>,
}

impl MergeCommand {
    /// Executes the command: merges, writes the file, and reports on stderr.
    ///
    /// `repo` is the repository location resolved at the CLI boundary
    /// (`None` = current working directory).
    pub fn execute(self, repo: Option<&Path>) -> Result<()> {
        let outcome = self.run(repo)?;
        for warning in &outcome.warnings {
            eprintln!("warning: {warning}");
        }
        eprintln!(
            "merged {} report(s) into {}: {} file(s), {} of {} lines covered ({})",
            outcome.inputs,
            outcome.output.display(),
            outcome.files,
            outcome.covered_lines,
            outcome.total_lines,
            // The same rounding `coverage diff` prints its total with.
            pct(outcome.percent),
        );
        Ok(())
    }

    /// Merges the reports and writes the result, without printing.
    ///
    /// `repo_root` is the location relative `REPORT` and `--output` paths are
    /// anchored to, and where the default strip prefix is looked for (`None`
    /// defaults to `.`). Every input is read and checked before the output is
    /// touched, so a failed merge writes nothing.
    pub fn run(&self, repo_root: Option<&Path>) -> Result<MergeOutcome> {
        ensure!(
            !self.report.is_empty(),
            "at least one coverage report is required"
        );
        let prefix = self
            .strip_prefix
            .clone()
            .or_else(|| repo_workdir(repo_root.unwrap_or_else(|| Path::new("."))));

        let mut warnings = Vec::new();
        let mut merged = CoverageReport::new();
        for path in &self.report {
            let path = resolve(path, repo_root);
            let mut report = read_report(&path, self.report_format)?;
            // Unlike `coverage diff`, a lone input is checked too: a merge's
            // output is trusted by whatever reads it next, and nothing else
            // would notice it came from a run that measured nothing.
            let label = path.display().to_string();
            require_executable_lines(&label, &report)?;
            if let Some(prefix) = prefix.as_deref() {
                warnings.extend(prefix_mismatch(&label, &report, prefix));
                report.strip_prefix(prefix);
            }
            merged.merge(report);
        }

        let text = lcov::write(&merged)?;
        let output = resolve(&self.output, repo_root);
        write_atomically(&output, &text)?;

        Ok(MergeOutcome {
            output,
            inputs: self.report.len(),
            files: merged
                .files
                .values()
                .filter(|file| file.total_lines() > 0)
                .count(),
            total_lines: merged.total_lines(),
            covered_lines: merged.covered_lines(),
            percent: merged.percent(),
            warnings,
        })
    }
}

/// `path` anchored to `repo_root`, or exactly as given when there is none, so a
/// message names a file the way the user typed it.
fn resolve(path: &Path, repo_root: Option<&Path>) -> PathBuf {
    repo_root.map_or_else(|| path.to_path_buf(), |root| anchor(path, root))
}

/// The working directory of the repository containing `root`, if there is one.
fn repo_workdir(root: &Path) -> Option<PathBuf> {
    Repository::discover(root)
        .ok()?
        .workdir()
        .map(Path::to_path_buf)
}

/// Writes `contents` to `path` through a same-directory temp file and a rename,
/// so a reader never sees a partial file and a failed write never replaces one.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let name = path
        .file_name()
        .with_context(|| format!("{} is not a file path", path.display()))?;
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("could not write merged report to {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::coverage::diff::DiffCommand;
    use git2::Signature;
    use std::fs;
    use tempfile::TempDir;

    /// A repository whose second commit adds `b.rs`: the shape `coverage diff`
    /// is run against. Returns the dir, git2's canonical workdir (on macOS the
    /// tempdir `/var/...` is a symlink to `/private/var/...`, and the workdir is
    /// what the default strip prefix resolves to), and the base commit.
    fn repo_with_added_file() -> (TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        let commit = |files: &[(&str, &str)], parent: Option<git2::Oid>| {
            let mut index = repo.index().unwrap();
            index.clear().unwrap();
            for (name, content) in files {
                fs::write(workdir.join(name), content).unwrap();
                index.add_path(Path::new(name)).unwrap();
            }
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = Signature::now("Test", "test@example.com").unwrap();
            let parent = parent.map(|id| repo.find_commit(id).unwrap());
            let parents: Vec<&git2::Commit> = parent.as_ref().into_iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &parents)
                .unwrap()
        };
        let base = commit(&[("a.rs", "fn a() {}\n")], None);
        commit(
            &[("a.rs", "fn a() {}\n"), ("b.rs", "one\ntwo\nthree\n")],
            Some(base),
        );
        (dir, workdir, base.to_string())
    }

    /// Writes an lcov shard of `a.rs` and `b.rs` under `dir`, with absolute
    /// paths and — like real `cargo llvm-cov` output — no newline after the
    /// final `end_of_record`.
    fn write_shard(dir: &Path, name: &str, a: &[(u32, u64)], b: &[(u32, u64)]) -> PathBuf {
        let record = |file: &str, lines: &[(u32, u64)]| {
            let mut text = format!("TN:\nSF:{}\n", dir.join(file).display());
            for (line, hits) in lines {
                text.push_str(&format!("DA:{line},{hits}\n"));
            }
            text.push_str("end_of_record");
            text
        };
        let path = dir.join(name);
        fs::write(
            &path,
            format!("{}\n{}", record("a.rs", a), record("b.rs", b)),
        )
        .unwrap();
        path
    }

    fn merge(reports: Vec<PathBuf>, output: PathBuf) -> MergeCommand {
        MergeCommand {
            report: reports,
            report_format: ReportFormat::Auto,
            output,
            strip_prefix: None,
        }
    }

    /// The three shards used throughout: each covers different lines of `b.rs`.
    fn three_shards(repo: &Path) -> Vec<PathBuf> {
        vec![
            write_shard(repo, "one.lcov", &[(1, 1)], &[(1, 1), (2, 0), (3, 0)]),
            write_shard(repo, "two.lcov", &[(1, 0)], &[(1, 0), (2, 1), (3, 0)]),
            write_shard(repo, "three.lcov", &[(1, 0)], &[(1, 0), (2, 0), (3, 0)]),
        ]
    }

    fn diff_command(args: &[&str]) -> DiffCommand {
        let argv = std::iter::once("diff").chain(args.iter().copied());
        DiffCommand::try_parse_from(argv).unwrap()
    }

    // ── what the file says ───────────────────────────────────────────────

    #[test]
    fn writes_the_union_repo_relative_with_a_trailing_newline() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("merged.lcov");

        let outcome = merge(three_shards(&repo), out.clone())
            .run(Some(&repo))
            .unwrap();

        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "TN:\nSF:a.rs\nDA:1,1\nLF:1\nLH:1\nend_of_record\n\
             TN:\nSF:b.rs\nDA:1,1\nDA:2,1\nDA:3,0\nLF:3\nLH:2\nend_of_record\n"
        );
        assert_eq!(outcome.output, out);
        assert_eq!(outcome.inputs, 3);
        assert_eq!(outcome.files, 2);
        assert_eq!(outcome.total_lines, 4);
        assert_eq!(outcome.covered_lines, 3);
        assert_eq!(outcome.percent, Some(75.0));
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    /// The first acceptance criterion: the merged file carries the figure
    /// `coverage diff` computes from the shards themselves.
    #[test]
    fn the_merged_file_gives_the_total_diff_computes_from_the_shards() {
        let (_dir, repo, base) = repo_with_added_file();
        let shards = three_shards(&repo);
        let out = repo.join("merged.lcov");
        merge(shards.clone(), out.clone()).run(Some(&repo)).unwrap();

        let base_ref = ["--base-ref", base.as_str()];
        let mut from_shards = vec![];
        for shard in &shards {
            from_shards.extend(["--report", shard.to_str().unwrap()]);
        }
        from_shards.extend(base_ref);
        let from_merged = ["--report", out.to_str().unwrap(), base_ref[0], base_ref[1]];

        let by_shards = diff_command(&from_shards).run(Some(&repo)).unwrap();
        let by_merge = diff_command(&from_merged).run(Some(&repo)).unwrap();

        assert_eq!(by_merge.line_percent, Some(75.0));
        assert_eq!(by_merge.line_percent, by_shards.line_percent);
        assert_eq!(by_merge.patch_percent, by_shards.patch_percent);
        assert_eq!(by_merge.rendered, by_shards.rendered);
    }

    /// A merged file is usable as the single `--baseline-report` a sharded run
    /// could not otherwise provide.
    #[test]
    fn the_merged_file_works_as_a_baseline_report() {
        let (_dir, repo, base) = repo_with_added_file();
        let out = repo.join("merged.lcov");
        merge(three_shards(&repo), out.clone())
            .run(Some(&repo))
            .unwrap();
        let head = write_shard(&repo, "head.lcov", &[(1, 1)], &[(1, 1), (2, 1), (3, 1)]);

        let outcome = diff_command(&[
            "--report",
            head.to_str().unwrap(),
            "--baseline-report",
            out.to_str().unwrap(),
            "--base-ref",
            &base,
        ])
        .run(Some(&repo))
        .unwrap();

        // 3 of 4 lines before (`b.rs` at 2 of 3), all 4 after.
        assert_eq!(outcome.line_percent, Some(100.0));
        assert!(outcome.rendered.contains("66.67%"), "{}", outcome.rendered);
        assert!(outcome.rendered.contains("25 pp"), "{}", outcome.rendered);
    }

    /// The third: the same bytes for every argument order.
    #[test]
    fn output_is_byte_identical_for_every_input_order() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shards = three_shards(&repo);
        let mut outputs = Vec::new();
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let out = repo.join(format!("merged-{}.lcov", outputs.len()));
            let reports = order.iter().map(|&i| shards[i].clone()).collect();
            merge(reports, out.clone()).run(Some(&repo)).unwrap();
            outputs.push(fs::read(&out).unwrap());
        }
        assert!(
            outputs.windows(2).all(|pair| pair[0] == pair[1]),
            "order changed the bytes"
        );
    }

    /// The trap the command exists for: shards that end without a newline, as
    /// `cargo llvm-cov` writes them, must come out as separate, well-formed
    /// records that any line-oriented reader splits correctly.
    #[test]
    fn records_never_run_together_in_the_output() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("merged.lcov");
        let shards = three_shards(&repo);
        for shard in &shards {
            assert!(!fs::read_to_string(shard).unwrap().ends_with('\n'));
        }

        merge(shards, out.clone()).run(Some(&repo)).unwrap();

        let text = fs::read_to_string(&out).unwrap();
        assert!(text.ends_with("end_of_record\n"));
        assert_eq!(text.matches("end_of_record").count(), 2);
        for line in text.lines() {
            assert!(
                line == "TN:"
                    || line == "end_of_record"
                    || ["SF:", "DA:", "LF:", "LH:"]
                        .iter()
                        .any(|tag| line.starts_with(tag)),
                "{line:?} is not one record"
            );
        }
    }

    #[test]
    fn shards_of_different_formats_are_merged() {
        let (_dir, repo, _base) = repo_with_added_file();
        let lcov = write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1), (2, 0)]);
        let json = repo.join("two.json");
        fs::write(
            &json,
            serde_json::json!({
                "data": [{ "files": [{
                    "filename": repo.join("b.rs").display().to_string(),
                    "segments": [
                        [2, 1, 4, true, true, false],
                        [2, 9, 0, false, false, false],
                        [3, 1, 0, true, true, false],
                        [3, 9, 0, false, false, false]
                    ]
                }]}],
                "type": "llvm.coverage.json.export",
                "version": "2.0.1"
            })
            .to_string(),
        )
        .unwrap();
        let xml = repo.join("three.xml");
        fs::write(
            &xml,
            format!(
                r#"<coverage><packages><package><classes><class filename="{}"><lines><line number="9" hits="2"/></lines></class></classes></package></packages></coverage>"#,
                repo.join("c.rs").display()
            ),
        )
        .unwrap();
        let out = repo.join("merged.lcov");

        merge(vec![lcov, json, xml], out.clone())
            .run(Some(&repo))
            .unwrap();

        let text = fs::read_to_string(&out).unwrap();
        // `b.rs` line 2 is covered by the JSON shard only; line 3 exists only there.
        assert!(text.contains("SF:b.rs\nDA:1,1\nDA:2,4\nDA:3,0\n"), "{text}");
        assert!(text.contains("SF:c.rs\nDA:9,2\n"), "{text}");
    }

    #[test]
    fn an_explicit_report_format_applies_to_every_report() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shard = write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1)]);
        let mut cmd = merge(vec![shard], repo.join("merged.lcov"));
        cmd.report_format = ReportFormat::Cobertura;
        let message = format!("{:#}", cmd.run(Some(&repo)).unwrap_err());
        assert!(message.contains("one.lcov"), "{message}");
        assert!(!repo.join("merged.lcov").exists());
    }

    // ── the checks ───────────────────────────────────────────────────────

    /// Runs a merge expected to fail and returns its full error chain, asserting
    /// that the output was not created.
    fn failure(reports: Vec<PathBuf>, repo: &Path) -> String {
        let out = repo.join("merged.lcov");
        let error = merge(reports, out.clone())
            .run(Some(repo))
            .expect_err("the merge must fail");
        assert!(!out.exists(), "a failed merge must write nothing");
        format!("{error:#}")
    }

    #[test]
    fn a_shard_with_no_executable_lines_fails_by_name() {
        let (_dir, repo, _base) = repo_with_added_file();
        let good = write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1)]);
        // A parseable lcov whose blocks list no lines: the shape of a failed run.
        let bad = repo.join("two.lcov");
        fs::write(&bad, "TN:\nSF:b.rs\nend_of_record\n").unwrap();
        let message = failure(vec![good, bad], &repo);
        assert!(message.contains("two.lcov"), "{message}");
        assert!(message.contains("no executable lines"), "{message}");
    }

    #[test]
    fn an_unparseable_shard_fails_by_name() {
        let (_dir, repo, _base) = repo_with_added_file();
        let good = write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1)]);
        let bad = repo.join("two.lcov");
        fs::write(&bad, "").unwrap();
        assert!(failure(vec![good, bad], &repo).contains("two.lcov"));
    }

    #[test]
    fn a_missing_shard_fails_by_name() {
        let (_dir, repo, _base) = repo_with_added_file();
        let good = write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1)]);
        let message = failure(vec![good, repo.join("shard-2.lcov")], &repo);
        assert!(message.contains("shard-2.lcov"), "{message}");
    }

    /// A merge's output is trusted by whatever reads it next, so unlike
    /// `coverage diff` a lone empty input is refused as well.
    #[test]
    fn a_lone_report_with_no_lines_fails() {
        let (_dir, repo, _base) = repo_with_added_file();
        let bad = repo.join("only.lcov");
        fs::write(&bad, "TN:\nSF:b.rs\nend_of_record\n").unwrap();
        let message = failure(vec![bad], &repo);
        assert!(message.contains("only.lcov"), "{message}");
    }

    #[test]
    fn a_lone_report_is_rewritten_normalised() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shard = write_shard(&repo, "one.lcov", &[(1, 1)], &[(2, 0), (1, 5)]);
        let out = repo.join("merged.lcov");
        merge(vec![shard], out.clone()).run(Some(&repo)).unwrap();
        assert!(fs::read_to_string(&out)
            .unwrap()
            .contains("SF:b.rs\nDA:1,5\nDA:2,0\n"));
    }

    /// With no repository root given, a path is named the way it was typed — not
    /// as `./one.lcov` — so the message is recognisable.
    #[test]
    fn without_a_repository_root_a_path_is_named_as_typed() {
        let cmd = merge(
            vec![PathBuf::from("no-such-shard-2118.lcov")],
            PathBuf::from("no-such-output-2118.lcov"),
        );
        let message = format!("{:#}", cmd.run(None).unwrap_err());
        assert!(message.contains(" no-such-shard-2118.lcov"), "{message}");
        assert!(!Path::new("no-such-output-2118.lcov").exists());
    }

    #[test]
    fn no_reports_is_an_error() {
        let (_dir, repo, _base) = repo_with_added_file();
        let message = failure(Vec::new(), &repo);
        assert!(message.contains("at least one"), "{message}");
    }

    #[test]
    fn a_failed_merge_leaves_an_existing_output_alone() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("merged.lcov");
        fs::write(&out, "previous run\n").unwrap();
        let result = merge(vec![repo.join("missing.lcov")], out.clone()).run(Some(&repo));
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&out).unwrap(), "previous run\n");
    }

    #[test]
    fn a_shard_under_another_root_warns_by_name_but_is_still_merged() {
        let (_dir, repo, _base) = repo_with_added_file();
        let good = write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1)]);
        let foreign = repo.join("two.lcov");
        fs::write(
            &foreign,
            "SF:/home/runner/work/repo/src/a.rs\nDA:1,1\nend_of_record\n",
        )
        .unwrap();
        let out = repo.join("merged.lcov");

        let outcome = merge(vec![good, foreign], out.clone())
            .run(Some(&repo))
            .unwrap();

        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(outcome.warnings[0].contains("two.lcov"));
        assert!(fs::read_to_string(&out).unwrap().contains("SF:a.rs\n"));
    }

    #[test]
    fn a_lone_input_under_another_root_also_warns() {
        let (_dir, repo, _base) = repo_with_added_file();
        let foreign = repo.join("one.lcov");
        fs::write(
            &foreign,
            "SF:/home/runner/work/repo/src/a.rs\nDA:1,1\nend_of_record\n",
        )
        .unwrap();
        let outcome = merge(vec![foreign], repo.join("merged.lcov"))
            .run(Some(&repo))
            .unwrap();
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
    }

    // ── paths and where the file goes ────────────────────────────────────

    #[test]
    fn strip_prefix_overrides_the_repository_root() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shard = repo.join("one.lcov");
        fs::write(&shard, "SF:/ci/root/src/a.rs\nDA:1,1\nend_of_record\n").unwrap();
        let out = repo.join("merged.lcov");
        let mut cmd = merge(vec![shard], out.clone());
        cmd.strip_prefix = Some(PathBuf::from("/ci/root"));

        let outcome = cmd.run(Some(&repo)).unwrap();

        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert!(fs::read_to_string(&out).unwrap().contains("SF:src/a.rs\n"));
    }

    #[test]
    fn outside_a_repository_paths_are_written_as_the_reports_have_them() {
        let dir = tempfile::tempdir().unwrap();
        let shard = dir.path().join("one.lcov");
        fs::write(&shard, "SF:/ci/root/src/a.rs\nDA:1,1\nend_of_record\n").unwrap();
        let out = dir.path().join("merged.lcov");

        let outcome = merge(vec![shard], out.clone())
            .run(Some(dir.path()))
            .unwrap();

        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert!(fs::read_to_string(&out)
            .unwrap()
            .contains("SF:/ci/root/src/a.rs\n"));
    }

    #[test]
    fn relative_paths_are_anchored_to_the_repository_root() {
        let (_dir, repo, _base) = repo_with_added_file();
        write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1)]);

        merge(
            vec![PathBuf::from("one.lcov")],
            PathBuf::from("out/merged.lcov"),
        )
        .run(Some(&repo))
        .expect_err("`out/` does not exist, and must not be created");

        fs::create_dir(repo.join("out")).unwrap();
        merge(
            vec![PathBuf::from("one.lcov")],
            PathBuf::from("out/merged.lcov"),
        )
        .run(Some(&repo))
        .unwrap();
        assert!(repo.join("out/merged.lcov").is_file());
    }

    #[test]
    fn the_output_may_replace_an_input() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shards = three_shards(&repo);
        let target = shards[0].clone();

        merge(shards, target.clone()).run(Some(&repo)).unwrap();

        let text = fs::read_to_string(&target).unwrap();
        assert!(text.contains("DA:2,1\n"), "{text}");
        assert!(text.ends_with("end_of_record\n"));
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("merged.lcov");
        merge(three_shards(&repo), out).run(Some(&repo)).unwrap();
        let strays: Vec<_> = fs::read_dir(&repo)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn an_unwritable_output_is_an_error_naming_it() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("no-such-dir").join("merged.lcov");
        let error = merge(three_shards(&repo), out)
            .run(Some(&repo))
            .expect_err("the directory does not exist");
        assert!(format!("{error:#}").contains("no-such-dir"), "{error:#}");
        assert!(!repo.join("no-such-dir").exists());
    }

    #[test]
    fn an_output_with_no_file_name_is_an_error() {
        let (_dir, repo, _base) = repo_with_added_file();
        let error = merge(three_shards(&repo), PathBuf::from("/"))
            .run(Some(&repo))
            .expect_err("`/` is not a file");
        assert!(
            format!("{error:#}").contains("not a file path"),
            "{error:#}"
        );
    }

    #[test]
    fn execute_writes_the_file() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("merged.lcov");
        merge(three_shards(&repo), out.clone())
            .execute(Some(&repo))
            .unwrap();
        assert!(out.is_file());
    }

    #[test]
    fn execute_fails_on_a_bad_shard() {
        let (_dir, repo, _base) = repo_with_added_file();
        let cmd = merge(vec![repo.join("missing.lcov")], repo.join("merged.lcov"));
        assert!(cmd.execute(Some(&repo)).is_err());
    }

    // ── the command line ─────────────────────────────────────────────────

    #[test]
    fn parses_several_reports_and_an_output() {
        let cmd = MergeCommand::try_parse_from([
            "merge",
            "a.lcov",
            "b.lcov",
            "c.json",
            "-o",
            "merged.lcov",
        ])
        .unwrap();
        assert_eq!(cmd.report.len(), 3);
        assert_eq!(cmd.output, PathBuf::from("merged.lcov"));
        assert_eq!(cmd.report_format, ReportFormat::Auto);
        assert_eq!(cmd.strip_prefix, None);
    }

    #[test]
    fn requires_a_report_and_an_output() {
        assert!(MergeCommand::try_parse_from(["merge", "-o", "merged.lcov"]).is_err());
        assert!(MergeCommand::try_parse_from(["merge", "a.lcov"]).is_err());
    }
}
