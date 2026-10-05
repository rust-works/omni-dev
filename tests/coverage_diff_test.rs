#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for diff/patch coverage attribution against real git diffs.
//!
//! Each test builds a temporary git repository, commits a base and a head
//! revision, computes the `DiffModel` from `git2`, pairs it with a hand-built
//! per-line coverage report, and asserts the attribution. Covers the four
//! scenarios the feature must handle: a new file, a modified file with mixed
//! coverage, a line shift on otherwise-unchanged content, and a rename.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use git2::{Repository, Signature};
use tempfile::TempDir;

use omni_dev::coverage::{analyze, CoverageReport, DiffModel, DiffScope, FileCoverage};

/// A temporary git repo whose commits each define the full file set (files
/// omitted from a commit are removed), so renames and deletions work.
struct TestRepo {
    _temp_dir: TempDir,
    repo_path: PathBuf,
    repo: Repository,
    commits: Vec<git2::Oid>,
}

impl TestRepo {
    fn new() -> Result<Self> {
        let tmp_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        fs::create_dir_all(&tmp_root)?;
        let temp_dir = tempfile::tempdir_in(&tmp_root)?;
        let repo_path = temp_dir.path().to_path_buf();
        let repo = Repository::init(&repo_path)?;
        let mut config = repo.config()?;
        config.set_str("user.name", "Test User")?;
        config.set_str("user.email", "test@example.com")?;
        Ok(Self {
            _temp_dir: temp_dir,
            repo_path,
            repo,
            commits: Vec::new(),
        })
    }

    /// Commits exactly `files` (path, content); any previously committed file
    /// not listed is removed in this commit.
    fn commit(&mut self, message: &str, files: &[(&str, &str)]) -> Result<git2::Oid> {
        // Materialise the working tree to match `files` exactly.
        let mut index = self.repo.index()?;
        index.clear()?;
        for (name, content) in files {
            let path = self.repo_path.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, content)?;
            index.add_path(Path::new(name))?;
        }
        index.write()?;

        let signature = Signature::now("Test User", "test@example.com")?;
        let tree = self.repo.find_tree(index.write_tree()?)?;
        let parent = self.commits.last().map(|id| self.repo.find_commit(*id));
        let parent = match parent {
            Some(p) => Some(p?),
            None => None,
        };
        let parents: Vec<&git2::Commit> = parent.as_ref().into_iter().collect();
        let id = self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )?;
        self.commits.push(id);
        Ok(id)
    }

    fn base_sha(&self) -> String {
        self.commits[0].to_string()
    }

    fn diff(&self) -> Result<DiffModel> {
        DiffModel::between(&self.repo, &self.base_sha(), Some("HEAD"))
    }
}

/// Builds a coverage report from `(path, &[(line, hits)])` tuples.
fn cov(files: &[(&str, &[(u32, u64)])]) -> CoverageReport {
    let mut report = CoverageReport::new();
    for (path, lines) in files {
        let mut f = FileCoverage::new(*path);
        for &(n, h) in *lines {
            f.record(n, h);
        }
        report.insert(f);
    }
    report
}

#[test]
fn new_file_patch_coverage() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.commit("base", &[("a.rs", "fn a() {}\n")])?;
    repo.commit(
        "add b",
        &[("a.rs", "fn a() {}\n"), ("b.rs", "one\ntwo\nthree\n")],
    )?;

    let diff = repo.diff()?;
    let bdiff = diff.files.get("b.rs").expect("b.rs in diff");
    assert!(bdiff.is_new);
    assert_eq!(
        bdiff.added.iter().copied().collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    // Head report: line 1 & 3 covered, line 2 uncovered.
    let head = cov(&[("b.rs", &[(1, 1), (2, 0), (3, 4)])]);
    let result = analyze(&head, &diff, None, DiffScope::DiffOnly);
    assert_eq!(result.patch.covered, 2);
    assert_eq!(result.patch.uncovered, 1);
    assert_eq!(result.uncovered_new_lines, vec![("b.rs".to_string(), 2)]);
    Ok(())
}

#[test]
fn modified_file_mixed_coverage() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.commit("base", &[("m.rs", "a\nb\nc\n")])?;
    // Insert X, Y between b and c → new lines 3 and 4.
    repo.commit("modify", &[("m.rs", "a\nb\nX\nY\nc\n")])?;

    let diff = repo.diff()?;
    let mdiff = diff.files.get("m.rs").expect("m.rs in diff");
    assert!(!mdiff.is_new);
    assert_eq!(mdiff.added.iter().copied().collect::<Vec<_>>(), vec![3, 4]);

    // New line 3 covered, new line 4 uncovered.
    let head = cov(&[("m.rs", &[(1, 1), (2, 1), (3, 1), (4, 0), (5, 1)])]);
    let result = analyze(&head, &diff, None, DiffScope::DiffOnly);
    assert_eq!(result.patch.covered, 1);
    assert_eq!(result.patch.uncovered, 1);
    assert_eq!(result.uncovered_new_lines, vec![("m.rs".to_string(), 4)]);
    Ok(())
}

#[test]
fn line_shift_detects_indirect_change() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.commit("base", &[("s.rs", "l1\nl2\nl3\nl4\nl5\n")])?;
    // Insert two lines after l1: old l3 (line 3) shifts to new line 5.
    repo.commit("shift", &[("s.rs", "l1\nNEW1\nNEW2\nl2\nl3\nl4\nl5\n")])?;

    let diff = repo.diff()?;
    let sdiff = diff.files.get("s.rs").expect("s.rs in diff");
    // The two inserted lines are the added lines.
    assert_eq!(sdiff.added.iter().copied().collect::<Vec<_>>(), vec![2, 3]);
    // Unchanged content line l3 maps old line 3 → new line 5.
    assert_eq!(sdiff.map_base_to_head(3), Some(5));

    // Baseline: l3 covered. Head: same line (now line 5) uncovered.
    let baseline = cov(&[("s.rs", &[(3, 2)])]);
    let head = cov(&[("s.rs", &[(2, 1), (3, 1), (5, 0)])]);
    let result = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);

    assert_eq!(result.indirect.len(), 1, "expected one indirect flip");
    let change = &result.indirect[0];
    assert_eq!(change.path, "s.rs");
    assert_eq!(change.base_line, 3);
    assert_eq!(change.head_line, 5);
    assert!(!change.became_covered, "l3 lost coverage");
    Ok(())
}

#[test]
fn rename_does_not_show_false_lost_coverage() -> Result<()> {
    let mut repo = TestRepo::new()?;
    let body = "fn f() {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n";
    repo.commit("base", &[("old.rs", body)])?;
    // Rename old.rs → new.rs with identical content.
    repo.commit("rename", &[("new.rs", body)])?;

    let diff = repo.diff()?;
    let rdiff = diff.files.get("new.rs").expect("new.rs in diff");
    assert!(rdiff.is_rename, "rename should be detected");
    assert_eq!(rdiff.old_path.as_deref(), Some("old.rs"));
    assert!(rdiff.added.is_empty(), "no lines added by a pure rename");

    // Same coverage before (old path) and after (new path) ⇒ no flips.
    let baseline = cov(&[("old.rs", &[(2, 1), (3, 1), (4, 1)])]);
    let head = cov(&[("new.rs", &[(2, 1), (3, 1), (4, 1)])]);
    let result = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);
    assert_eq!(result.patch.total(), 0, "a pure rename adds no lines");
    assert!(
        result.indirect.is_empty(),
        "identical content must not look like lost coverage"
    );
    Ok(())
}

/// Exercises real producer reports through the command's config and path pipeline.
fn producer_fixture(
    fixture: &str,
    git_path: &str,
    from: &str,
    to: &str,
    covered: u64,
    total: u64,
) -> Result<()> {
    use clap::Parser;
    use omni_dev::cli::coverage::{CoverageCommand, CoverageSubcommands};

    let mut repo = TestRepo::new()?;
    repo.commit("base", &[("base.txt", "base\n")])?;
    let source = "line\n".repeat(40);
    repo.commit("add source", &[("base.txt", "base\n"), (git_path, &source)])?;
    let config = repo.repo_path.join(".omni-dev");
    fs::create_dir(&config)?;
    fs::write(config.join("coverage.yaml"), "{}\n")?;
    let report = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/coverage/non-rust")
        .join(fixture);
    let base = repo.base_sha();
    let command = CoverageCommand::try_parse_from([
        "coverage",
        "diff",
        "--report",
        report.to_str().unwrap(),
        "--base-ref",
        &base,
        "--context-dir",
        config.to_str().unwrap(),
        "-o",
        "json",
    ])?;
    let CoverageSubcommands::Diff(cmd) = command.command else {
        unreachable!("parsed a diff command")
    };
    // Overall coverage can exist while a path mismatch leaves the patch empty.
    assert_eq!(
        cmd.run(Some(&repo.repo_path))?.patch_percent,
        None,
        "{fixture}"
    );
    fs::write(
        config.join("coverage.yaml"),
        format!("diff:\n  path-mappings:\n    - from: '{from}'\n      to: '{to}'\n"),
    )?;
    let result = cmd.run(Some(&repo.repo_path))?;
    assert_eq!(
        result.patch_percent,
        Some(covered as f64 / total as f64 * 100.0),
        "{fixture}"
    );
    let json: serde_json::Value = serde_json::from_str(&result.rendered)?;
    assert_eq!(json["patch_coverage"]["total"], total, "{fixture}");
    assert_eq!(json["patch_coverage"]["covered"], covered, "{fixture}");
    assert_eq!(
        json["patch_coverage"]["files"][0]["path"], git_path,
        "{fixture}"
    );
    Ok(())
}

#[test]
fn nyc_monorepo_patch_attribution() -> Result<()> {
    producer_fixture(
        "nyc.lcov",
        "packages/web/src/calc.js",
        "src",
        "packages/web/src",
        2,
        3,
    )
}

#[test]
fn coverage_py_source_root_patch_attribution() -> Result<()> {
    producer_fixture(
        "coverage-py.xml",
        "services/python/src/calc.py",
        "",
        "services/python",
        4,
        5,
    )
}

#[test]
fn gcovr_source_root_patch_attribution() -> Result<()> {
    producer_fixture("gcovr.xml", "native/example.cpp", "", "native", 6, 7)
}

#[test]
fn coverlet_source_root_patch_attribution() -> Result<()> {
    producer_fixture(
        "coverlet.xml",
        "src/dotnet/AbstractClass.cs",
        "",
        "src/dotnet",
        4,
        8,
    )
}

#[test]
fn dart_runner_root_patch_attribution() -> Result<()> {
    producer_fixture(
        "dart.lcov",
        "packages/mobile/lib/calc.dart",
        "/private/tmp/2188-producers/dart",
        "packages/mobile",
        1,
        2,
    )
}

#[test]
fn jacoco_source_root_mapping_attributes_patch() -> Result<()> {
    use omni_dev::coverage::paths::PathMapping;
    let mut repo = TestRepo::new()?;
    repo.commit("base", &[("base.txt", "base\n")])?;
    repo.commit(
        "add java",
        &[(
            "services/api/src/main/java/com/example/App.java",
            "one\ntwo\n",
        )],
    )?;
    let mut head = omni_dev::coverage::parse(
        r#"<report name="app"><package name="com/example"><sourcefile name="App.java"><line nr="1" mi="0" ci="2"/><line nr="2" mi="1" ci="0"/></sourcefile></package></report>"#,
        None,
    )?;
    head.map_paths(&[PathMapping {
        from: String::new(),
        to: "services/api/src/main/java".into(),
    }])?;
    let result = analyze(&head, &repo.diff()?, None, DiffScope::DiffOnly);
    assert_eq!(result.patch.covered, 1);
    assert_eq!(result.patch.uncovered, 1);
    assert_eq!(
        result.uncovered_new_lines,
        vec![("services/api/src/main/java/com/example/App.java".into(), 2)]
    );
    Ok(())
}
