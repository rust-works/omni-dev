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
    lint_with(root, &[], paths)
}

fn lint_with(root: &Path, flags: &[&str], paths: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .env_remove("OMNI_DEV_CONFIG_DIR")
        .args(["coverage", "lint-markers", "-C"])
        .arg(root)
        .args(flags)
        .args(paths)
        .output()
        .unwrap()
}

fn err_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn bad() -> String {
    marker("ignore reason=\"unclosed\"")
}

/// Declares `lint-markers.include` in the repo's `.omni-dev/coverage.yaml`.
fn config_include(repo: &Repository, globs: &[&str]) {
    let mut body = String::from("lint-markers:\n  include:\n");
    for glob in globs {
        body.push_str(&format!("    - '{glob}'\n"));
    }
    write(repo, ".omni-dev/coverage.yaml", &body, false);
}

fn marker(rest: &str) -> String {
    format!("// {} {rest}\n", omni_dev::coverage::markers::INTRODUCER)
}

#[test]
fn default_scans_tracked_worktree_files_only() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "src/clean.rs", "fn clean() {}\n", true);
    write(
        &repo,
        "src/untracked.rs",
        &marker("ignore reason=\"unclosed\""),
        false,
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

#[test]
fn default_scans_non_rust_tracked_files() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "src/clean.rs", "fn clean() {}\n", true);
    write(&repo, "scripts/tool.py", &bad().replace("//", "#"), true);
    write(&repo, "notes.txt", &bad(), false);

    let output = lint(root, &[]);
    assert!(!output.status.success());
    let stderr = err_text(&output);
    assert!(
        stderr.contains("scripts/tool.py:1: unterminated"),
        "{stderr}"
    );
    assert!(!stderr.contains("notes.txt"), "{stderr}");
}

#[test]
fn binary_files_are_skipped_by_default_and_by_path() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "ok.txt", "plain\n", true);
    let mut bytes = vec![0xff, 0xfe, 0x00, b'\n'];
    bytes.extend_from_slice(bad().as_bytes());
    let file = root.join("blob.bin");
    std::fs::write(&file, &bytes).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("blob.bin")).unwrap();
    index.write().unwrap();

    assert!(lint(root, &[]).status.success());
    assert!(lint(root, &["blob.bin"]).status.success());
}

#[test]
fn a_missing_explicit_path_is_still_an_error() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "ok.txt", "plain\n", true);
    let output = lint(root, &["absent.txt"]);
    assert!(!output.status.success());
    assert!(
        err_text(&output).contains("could not read"),
        "{}",
        err_text(&output)
    );
}

#[test]
fn gitlinks_and_symlinks_are_not_read() {
    use git2::{IndexEntry, IndexTime, Oid};

    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "ok.txt", "plain\n", true);
    // A submodule is a directory in the worktree; reading it would fail.
    std::fs::create_dir_all(root.join("vendor/lib")).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add(&IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160_000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: Oid::from_str("0123456789012345678901234567890123456789").unwrap(),
            flags: 0,
            flags_extended: 0,
            path: b"vendor/lib".to_vec(),
        })
        .unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("vendor", root.join("link")).unwrap();
        index.add_path(Path::new("link")).unwrap();
    }
    index.write().unwrap();

    let output = lint(root, &[]);
    assert!(output.status.success(), "{}", err_text(&output));
}

#[test]
fn include_narrows_the_default_scan_and_repeats() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "src/a.py", &bad(), true);
    write(&repo, "src/deep/b.py", &bad(), true);
    write(&repo, "docs/c.md", &bad(), true);
    write(&repo, "d.rs", &bad(), true);

    let output = lint_with(root, &["--include", "src/**/*.py"], &[]);
    assert!(!output.status.success());
    let stderr = err_text(&output);
    assert!(stderr.contains("src/a.py:1"), "{stderr}");
    assert!(stderr.contains("src/deep/b.py:1"), "{stderr}");
    assert!(!stderr.contains("docs/c.md"), "{stderr}");
    assert!(!stderr.contains("d.rs"), "{stderr}");

    // `*` does not cross a directory separator, and the flag repeats.
    let output = lint_with(root, &["--include", "*.rs", "--include", "docs/*.md"], &[]);
    let stderr = err_text(&output);
    assert!(stderr.contains("d.rs:1"), "{stderr}");
    assert!(stderr.contains("docs/c.md:1"), "{stderr}");
    assert!(!stderr.contains("src/"), "{stderr}");

    // A glob that matches nothing scans nothing.
    assert!(lint_with(root, &["--include", "*.go"], &[])
        .status
        .success());
}

#[test]
fn include_does_not_restrict_explicit_paths() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "notes.txt", &bad(), true);
    let output = lint_with(root, &["--include", "*.rs"], &["notes.txt"]);
    assert!(!output.status.success());
    assert!(
        err_text(&output).contains("notes.txt:1"),
        "{}",
        err_text(&output)
    );
}

#[test]
fn invalid_include_glob_is_an_error() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "a.rs", "fn a() {}\n", true);
    let output = lint_with(root, &["--include", "src/["], &[]);
    assert!(!output.status.success());
    let stderr = err_text(&output);
    assert!(stderr.contains("invalid glob `src/[`"), "{stderr}");
    assert!(stderr.contains("--include"), "{stderr}");
}

#[test]
fn config_include_narrows_the_default_scan() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "a.rs", &bad(), true);
    write(&repo, "docs/b.md", &bad(), true);
    config_include(&repo, &["**/*.rs"]);

    let output = lint(root, &[]);
    assert!(!output.status.success());
    let stderr = err_text(&output);
    assert!(stderr.contains("a.rs:1"), "{stderr}");
    assert!(!stderr.contains("docs/b.md"), "{stderr}");

    // Narrowed away entirely: nothing to report.
    config_include(&repo, &["**/*.go"]);
    assert!(lint(root, &[]).status.success());
}

#[test]
fn include_flag_replaces_the_config_list() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "a.rs", &bad(), true);
    write(&repo, "docs/b.md", &bad(), true);
    config_include(&repo, &["**/*.rs"]);

    let output = lint_with(root, &["--include", "docs/*.md"], &[]);
    let stderr = err_text(&output);
    assert!(stderr.contains("docs/b.md:1"), "{stderr}");
    assert!(!stderr.contains("a.rs"), "{stderr}");
}

#[test]
fn malformed_config_and_invalid_config_glob_are_errors() {
    let (_dir, repo) = repo();
    let root = repo.workdir().unwrap();
    write(&repo, "a.rs", "fn a() {}\n", true);

    write(
        &repo,
        ".omni-dev/coverage.yaml",
        "lint-markers: [oops\n",
        false,
    );
    let output = lint(root, &[]);
    assert!(!output.status.success());
    assert!(
        err_text(&output).contains("could not parse coverage config"),
        "{}",
        err_text(&output)
    );

    config_include(&repo, &["src/["]);
    let output = lint(root, &[]);
    assert!(!output.status.success());
    assert!(
        err_text(&output).contains("lint-markers.include in coverage.yaml"),
        "{}",
        err_text(&output)
    );

    // Explicit paths never read the config, so a broken one cannot block them.
    assert!(lint(root, &["a.rs"]).status.success());
}
