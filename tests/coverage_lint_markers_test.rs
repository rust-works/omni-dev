//! CLI behavior for standalone coverage marker linting.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

use git2::Repository;
use tempfile::TempDir;

fn repo() -> (TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    (dir, repo)
}

fn write(repo: &Repository, path: &str, source: &str, track: bool) {
    let root = repo.workdir().unwrap();
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, source).unwrap();
    if track {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(path)).unwrap();
        index.write().unwrap();
    }
}

fn lint(root: &Path, paths: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["coverage", "lint-markers", "-C"])
        .arg(root)
        .args(paths)
        .output()
        .unwrap()
}

fn marker(rest: &str) -> String {
    format!("// {} {rest}\n", omni_dev::coverage::markers::INTRODUCER)
}

#[test]
fn default_scans_tracked_rust_worktree_files_only() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "src/clean.rs", "fn clean() {}\n", true);
    write(
        &repo,
        "src/untracked.rs",
        &marker("ignore reason=\"unclosed\""),
        false,
    );
    write(
        &repo,
        "src/other.txt",
        &marker("ignore reason=\"unclosed\""),
        true,
    );
    assert!(lint(root, &[]).status.success());

    // The index still tracks this file, but lint reads the edited worktree.
    write(
        &repo,
        "src/clean.rs",
        &marker("ignore reason=\"unclosed\""),
        false,
    );
    let output = lint(root, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("src/clean.rs:1: unterminated"), "{stderr}");
    assert!(!stderr.contains("src/untracked.rs"), "{stderr}");
}

#[test]
fn explicit_paths_select_files_and_report_unclosed_reason_quotes() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "src/clean.rs", "fn clean() {}\n", true);
    write(
        &repo,
        "src/bad.rs",
        &marker("ignore-line reason=\"unclosed"),
        false,
    );

    assert!(lint(root, &["src/clean.rs"]).status.success());
    let output = lint(root, &["src/bad.rs"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("src/bad.rs:1: unterminated `reason="),
        "{stderr}"
    );
}

#[test]
fn reports_errors_in_more_than_one_file() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "a.rs", &marker("ignore reason=\"unclosed\""), true);
    write(
        &repo,
        "b.rs",
        &marker("ignore-line reason=\"unclosed"),
        true,
    );

    let output = lint(root, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("a.rs:1: unterminated"), "{stderr}");
    assert!(stderr.contains("b.rs:1: unterminated"), "{stderr}");
}
