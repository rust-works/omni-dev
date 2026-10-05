//! `omni-dev coverage merge` — one report from the shards of a sharded run.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use git2::Repository;

use super::diff::{anchor, read_report, ReportFormat};
use crate::coverage::merge::check_shard;
use crate::coverage::render::pct;
use crate::coverage::{lcov, CoverageReport};

/// The values `coverage diff -o` accepts, and `lcov` — what a user who mixes the
/// two commands up would type to `merge -o`.
const OUTPUT_FORMAT_NAMES: [&str; 4] = ["markdown", "yaml", "json", "lcov"];

/// How many temp names `write_atomically` tries before giving up.
const MAX_TEMP_ATTEMPTS: u32 = 100;

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
/// Each input may be lcov, llvm-cov JSON, Cobertura or a Go coverprofile, detected
/// per file. An input that is missing, unparseable or has no executable lines
/// fails the run, naming it, because a shard that silently produced nothing would only make
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
    /// Coverage reports to merge (lcov / llvm-cov-json / cobertura / go-coverprofile), one per shard.
    #[arg(value_name = "REPORT", required = true)]
    pub report: Vec<PathBuf>,

    /// Format of every report (auto-detected per file by default).
    #[arg(long, value_enum, default_value_t = ReportFormat::Auto)]
    pub report_format: ReportFormat,

    /// File to write the merged lcov report to.
    ///
    /// Unlike `coverage diff -o`, which selects an output *format*, this is a
    /// path: a bare `markdown`, `yaml`, `json` or `lcov` is refused as a likely
    /// mistake (write `./json` for a file of that name). The file is replaced
    /// atomically, through a symlink if it is one and keeping its mode: a merge
    /// that fails leaves an existing file untouched, and a path that is also an
    /// input is allowed.
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
    /// Number of files in the merged report.
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
        // `coverage diff -o` takes a format, so `-o json` is a likely slip, and
        // would otherwise write a file called `json` and exit 0.
        for format in OUTPUT_FORMAT_NAMES {
            if self.output == Path::new(format) {
                bail!(
                    "-o/--output is the file to write, but `{format}` looks like an output \
                     format (`coverage diff -o` selects one); to write a file with that name, \
                     pass `./{format}`"
                );
            }
        }
        let prefix = match &self.strip_prefix {
            Some(prefix) => Some(prefix.clone()),
            None => repo_workdir(repo_root.unwrap_or_else(|| Path::new(".")))?,
        };

        let mut warnings = Vec::new();
        let mut merged = CoverageReport::new();
        // `go.mod` is looked up where the default strip prefix is: the repository's
        // working directory, so running from a subdirectory finds the root's.
        let root = match (&self.strip_prefix, &prefix) {
            (None, Some(workdir)) => workdir.as_path(),
            _ => repo_root.unwrap_or_else(|| Path::new(".")),
        };
        for path in &self.report {
            let path = resolve(path, repo_root);
            let mut report = read_report(&path, self.report_format, root)?;
            // Unlike `coverage diff`, a lone input is checked too: a merge's
            // output is trusted by whatever reads it next, and nothing else
            // would notice it came from a run that measured nothing.
            check_shard(
                &path.display().to_string(),
                &report,
                prefix.as_deref(),
                &mut warnings,
            )?;
            if let Some(prefix) = prefix.as_deref() {
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
            files: merged.files.len(),
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

/// The working directory of the repository containing `root`, or `None` when
/// `root` is not inside one (or the repository has no working tree).
///
/// Only "not a repository" means "no prefix". A repository that exists but
/// cannot be opened (an unreadable checkout, one owned by another user, as in
/// some CI containers) is an error: carrying on would write absolute runner
/// paths into the merged file with nothing to say so.
fn repo_workdir(root: &Path) -> Result<Option<PathBuf>> {
    match Repository::discover(root) {
        Ok(repo) => Ok(repo.workdir().map(Path::to_path_buf)),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "could not open the git repository at {}; pass --strip-prefix to merge without it",
                root.display()
            )
        }),
    }
}

/// Writes `contents` to `path` through a same-directory temp file and a rename,
/// so a reader never sees a partial file and a failed write never replaces one.
///
/// A symlink at `path` is written through rather than replaced, and an existing
/// file keeps its mode (a bare rename would reset it to the umask default). The
/// temp name carries a counter, so a leftover from a killed run that had the same
/// pid is stepped over instead of failing every later run.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let failed = || format!("could not write merged report to {}", path.display());
    // A path that does not exist yet has no symlink to follow.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target
        .file_name()
        .with_context(|| format!("{} is not a file path", path.display()))?;
    let mode = std::fs::metadata(&target)
        .map(|meta| meta.permissions())
        .ok();

    let mut attempt = 0_u32;
    let (tmp, mut file) = loop {
        let tmp = target.with_file_name(format!(
            ".{}.{}.{attempt}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && attempt < MAX_TEMP_ATTEMPTS =>
            {
                attempt += 1;
            }
            Err(error) => return Err(error).with_context(failed),
        }
    };
    let result = (|| -> std::io::Result<()> {
        file.write_all(contents.as_bytes())?;
        if let Some(mode) = mode {
            file.set_permissions(mode)?;
        }
        file.sync_all()?;
        // Closed before the rename, which some platforms refuse otherwise.
        drop(file);
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        // Best effort: the error reported is the write's, and a temp file left
        // behind is harmless, since the next run takes a different name.
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(failed)
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

    // ── Go coverprofiles ─────────────────────────────────────────────────

    /// Writes a Go coverprofile under `dir`, naming files by import path under
    /// `module` the way `go test -coverprofile` does.
    fn write_go_profile(dir: &Path, name: &str, module: &str, body: &[&str]) -> PathBuf {
        let mut text = String::from("mode: set\n");
        for block in body {
            text.push_str(&format!("{module}/{block}\n"));
        }
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    /// Three sharded profiles of `b.rs` (three lines) under `example.com/m`,
    /// each covering a different block, with a `go.mod` at the repository root.
    fn go_shards(repo: &Path) -> Vec<PathBuf> {
        fs::write(repo.join("go.mod"), "module example.com/m\n\ngo 1.22\n").unwrap();
        vec![
            write_go_profile(
                repo,
                "one.cov",
                "example.com/m",
                &["b.rs:1.1,1.9 1 1", "b.rs:2.1,3.9 2 0"],
            ),
            write_go_profile(
                repo,
                "two.cov",
                "example.com/m",
                &["b.rs:1.1,1.9 1 0", "b.rs:2.1,2.9 1 1"],
            ),
            write_go_profile(
                repo,
                "three.cov",
                "example.com/m",
                &["b.rs:1.1,1.9 1 0", "b.rs:2.1,3.9 2 0"],
            ),
        ]
    }

    #[test]
    fn sharded_go_profiles_merge_to_repo_relative_lcov() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("merged.lcov");

        let outcome = merge(go_shards(&repo), out.clone())
            .run(Some(&repo))
            .unwrap();

        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "TN:\nSF:b.rs\nDA:1,1\nDA:2,1\nDA:3,0\nLF:3\nLH:2\nend_of_record\n"
        );
        assert_eq!(outcome.files, 1);
        assert_eq!(outcome.total_lines, 3);
        assert_eq!(outcome.covered_lines, 2);
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    #[test]
    fn diff_over_sharded_go_profiles_gives_the_merged_files_figures() {
        let (_dir, repo, base) = repo_with_added_file();
        let shards = go_shards(&repo);
        let out = repo.join("merged.lcov");
        merge(shards.clone(), out.clone()).run(Some(&repo)).unwrap();

        let mut from_shards = vec![];
        for shard in &shards {
            from_shards.extend(["--report", shard.to_str().unwrap()]);
        }
        from_shards.extend(["--base-ref", base.as_str()]);
        let from_merged = [
            "--report",
            out.to_str().unwrap(),
            "--base-ref",
            base.as_str(),
        ];

        let by_shards = diff_command(&from_shards).run(Some(&repo)).unwrap();
        let by_merge = diff_command(&from_merged).run(Some(&repo)).unwrap();

        // `b.rs` is the whole diff: lines 1 and 2 covered by some shard, 3 by none.
        let patch = by_shards.patch_percent.unwrap();
        assert!((patch - 200.0 / 3.0).abs() < 1e-9, "{patch}");
        assert_eq!(by_shards.patch_percent, by_merge.patch_percent);
        assert_eq!(by_shards.rendered, by_merge.rendered);
    }

    #[test]
    fn a_go_profile_is_detected_and_may_be_named_explicitly() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shards = go_shards(&repo);
        let out = repo.join("merged.lcov");
        let mut cmd = merge(shards, out.clone());
        cmd.report_format = ReportFormat::GoCoverprofile;
        cmd.run(Some(&repo)).unwrap();
        assert!(fs::read_to_string(&out).unwrap().contains("SF:b.rs\n"));
    }

    #[test]
    fn the_go_mod_is_found_from_a_subdirectory_of_the_repository() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shards = go_shards(&repo);
        let sub = repo.join("ci");
        fs::create_dir(&sub).unwrap();
        let out = repo.join("merged.lcov");

        merge(shards, out.clone()).run(Some(&sub)).unwrap();

        assert!(fs::read_to_string(&out).unwrap().contains("SF:b.rs\n"));
    }

    #[test]
    fn go_paths_are_left_as_written_without_a_go_mod() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shard = write_go_profile(&repo, "one.cov", "example.com/m", &["b.rs:1.1,1.9 1 1"]);
        let out = repo.join("merged.lcov");

        merge(vec![shard], out.clone()).run(Some(&repo)).unwrap();

        assert!(fs::read_to_string(&out)
            .unwrap()
            .contains("SF:example.com/m/b.rs\n"));
    }

    #[test]
    fn go_paths_outside_the_go_mod_module_are_left_as_written() {
        let (_dir, repo, _base) = repo_with_added_file();
        fs::write(repo.join("go.mod"), "module example.com/other\n").unwrap();
        let shard = write_go_profile(&repo, "one.cov", "example.com/m", &["b.rs:1.1,1.9 1 1"]);
        let out = repo.join("merged.lcov");

        merge(vec![shard], out.clone()).run(Some(&repo)).unwrap();

        assert!(fs::read_to_string(&out)
            .unwrap()
            .contains("SF:example.com/m/b.rs\n"));
    }

    #[test]
    fn strip_prefix_maps_go_paths_when_there_is_no_go_mod_at_the_root() {
        let (_dir, repo, _base) = repo_with_added_file();
        // A module in a subdirectory: its import paths carry the subdirectory.
        let shard = write_go_profile(&repo, "one.cov", "example.com/m/svc", &["b.rs:1.1,1.9 1 1"]);
        let out = repo.join("merged.lcov");
        let mut cmd = merge(vec![shard], out.clone());
        cmd.strip_prefix = Some(PathBuf::from("example.com/m/svc"));

        cmd.run(Some(&repo)).unwrap();

        assert!(fs::read_to_string(&out).unwrap().contains("SF:b.rs\n"));
    }

    #[test]
    fn a_go_mod_is_not_read_for_other_formats() {
        let (_dir, repo, _base) = repo_with_added_file();
        fs::write(repo.join("go.mod"), "module b.rs\n").unwrap();
        let shard = write_shard(&repo, "one.lcov", &[(1, 1)], &[(1, 1)]);
        let out = repo.join("merged.lcov");

        merge(vec![shard], out.clone()).run(Some(&repo)).unwrap();

        assert!(fs::read_to_string(&out).unwrap().contains("SF:b.rs\n"));
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
        failure_to(reports, PathBuf::from("merged.lcov"), repo)
    }

    /// [`failure`] with a chosen `-o`, relative to `repo`.
    fn failure_to(reports: Vec<PathBuf>, output: PathBuf, repo: &Path) -> String {
        let error = merge(reports, output.clone())
            .run(Some(repo))
            .expect_err("the merge must fail");
        assert!(
            !repo.join(&output).exists(),
            "a failed merge must write nothing"
        );
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

    // ── review follow-ups ────────────────────────────────────────────────

    #[test]
    fn an_output_that_looks_like_a_format_is_refused() {
        let (_dir, repo, _base) = repo_with_added_file();
        for name in ["markdown", "yaml", "json", "lcov"] {
            let message = failure_to(three_shards(&repo), PathBuf::from(name), &repo);
            assert!(message.contains("looks like an output format"), "{message}");
            assert!(message.contains(&format!("./{name}")), "{message}");
            assert!(!repo.join(name).exists());
        }
    }

    #[test]
    fn a_format_name_is_fine_with_a_directory_part() {
        let (_dir, repo, _base) = repo_with_added_file();
        merge(three_shards(&repo), PathBuf::from("./json"))
            .run(Some(&repo))
            .unwrap();
        assert!(repo.join("json").is_file());
    }

    /// A file with no executable lines is a record like any other, so what is
    /// written reads back as exactly the merged report.
    #[test]
    fn an_empty_file_record_is_kept() {
        let (_dir, repo, _base) = repo_with_added_file();
        let shard = repo.join("two.lcov");
        fs::write(
            &shard,
            format!(
                "SF:{}\nDA:1,1\nend_of_record\nSF:{}\nend_of_record\n",
                repo.join("a.rs").display(),
                repo.join("c.rs").display()
            ),
        )
        .unwrap();
        let out = repo.join("merged.lcov");

        let outcome = merge(vec![shard], out.clone()).run(Some(&repo)).unwrap();

        assert_eq!(outcome.files, 2);
        let text = fs::read_to_string(&out).unwrap();
        assert!(
            text.contains("SF:c.rs\nLF:0\nLH:0\nend_of_record\n"),
            "{text}"
        );
    }

    /// A repository that is there but cannot be opened: libgit2 refuses a format
    /// version it does not know (a garbage `.git` file, by contrast, just reads
    /// as "not a repository").
    fn unusable_repository() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.config()
            .unwrap()
            .set_i32("core.repositoryformatversion", 99)
            .unwrap();
        dir
    }

    #[test]
    fn a_repository_that_cannot_be_opened_is_an_error_not_a_missing_prefix() {
        let dir = unusable_repository();
        let shard = dir.path().join("one.lcov");
        fs::write(&shard, "SF:/ci/root/src/a.rs\nDA:1,1\nend_of_record\n").unwrap();

        let error = merge(vec![shard], dir.path().join("merged.lcov"))
            .run(Some(dir.path()))
            .expect_err("an unusable repository must not silently disable stripping");

        assert!(format!("{error:#}").contains("--strip-prefix"), "{error:#}");
        assert!(!dir.path().join("merged.lcov").exists());
    }

    #[test]
    fn strip_prefix_needs_no_usable_repository() {
        let dir = unusable_repository();
        let shard = dir.path().join("one.lcov");
        fs::write(&shard, "SF:/ci/root/src/a.rs\nDA:1,1\nend_of_record\n").unwrap();
        let mut cmd = merge(vec![shard], dir.path().join("merged.lcov"));
        cmd.strip_prefix = Some(PathBuf::from("/ci/root"));
        cmd.run(Some(dir.path())).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_output_is_written_through() {
        let (_dir, repo, _base) = repo_with_added_file();
        let real = repo.join("real.lcov");
        let link = repo.join("link.lcov");
        fs::write(&real, "old\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        merge(three_shards(&repo), link.clone())
            .run(Some(&repo))
            .unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(fs::read_to_string(&real)
            .unwrap()
            .starts_with("TN:\nSF:a.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_outputs_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("merged.lcov");
        fs::write(&out, "old\n").unwrap();
        fs::set_permissions(&out, fs::Permissions::from_mode(0o600)).unwrap();

        merge(three_shards(&repo), out.clone())
            .run(Some(&repo))
            .unwrap();

        let mode = fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A temp file left by a killed run that had this pid must not block every
    /// later run: the next name is taken, and the leftover is not touched.
    #[test]
    fn a_stale_temporary_file_does_not_block_the_merge() {
        let (_dir, repo, _base) = repo_with_added_file();
        let stale = repo.join(format!(".merged.lcov.{}.0.tmp", std::process::id()));
        fs::write(&stale, "leftover").unwrap();

        merge(three_shards(&repo), repo.join("merged.lcov"))
            .run(Some(&repo))
            .unwrap();

        assert!(repo.join("merged.lcov").is_file());
        assert_eq!(fs::read_to_string(&stale).unwrap(), "leftover");
    }

    /// The rename is what fails here (a file cannot replace a directory), after
    /// the temp file exists: it must be cleaned up, and the directory untouched.
    #[test]
    fn a_failed_rename_leaves_no_temporary_file() {
        let (_dir, repo, _base) = repo_with_added_file();
        let out = repo.join("occupied");
        fs::create_dir(&out).unwrap();

        let error = merge(three_shards(&repo), out.clone())
            .run(Some(&repo))
            .expect_err("a directory cannot be replaced by a file");

        assert!(format!("{error:#}").contains("occupied"), "{error:#}");
        assert!(out.is_dir());
        let strays: Vec<_> = fs::read_dir(&repo)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
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
