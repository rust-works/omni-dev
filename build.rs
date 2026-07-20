//! Build script: captures git provenance (commit SHA, dirty flag, commit date)
//! plus a build timestamp at compile time, exposing them to the crate as
//! `OMNI_DEV_*` compile-time environment variables read by `src/build_info.rs`.
//!
//! Every value is best-effort: outside a git checkout (a crates.io download or a
//! release tarball has no `.git`) the corresponding variable is simply left
//! unset and the crate falls back gracefully. See issue #1374.

use std::env;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Runs `git` with `args`, returning trimmed stdout on success, or `None` when
/// git is absent, exits non-zero, or prints nothing (e.g. built outside a
/// checkout). Best-effort by design — a missing value is never an error.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Emits a `cargo:rustc-env=KEY=value` instruction when `value` is present.
fn emit(key: &str, value: Option<&str>) {
    if let Some(value) = value {
        println!("cargo:rustc-env={key}={value}");
    }
}

fn main() {
    emit("OMNI_DEV_GIT_SHA", git(&["rev-parse", "HEAD"]).as_deref());
    emit(
        "OMNI_DEV_GIT_SHA_SHORT",
        git(&["rev-parse", "--short", "HEAD"]).as_deref(),
    );
    emit(
        "OMNI_DEV_GIT_COMMIT_DATE",
        git(&["show", "-s", "--format=%cI", "HEAD"]).as_deref(),
    );

    // Working-tree cleanliness — only meaningful inside a work tree, so gate the
    // emission on that (a bare repo or non-checkout leaves the flag unset).
    if git(&["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true") {
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        println!("cargo:rustc-env=OMNI_DEV_GIT_DIRTY={dirty}");
    }

    // Build timestamp: honour SOURCE_DATE_EPOCH for reproducible builds, else the
    // wall clock. Stored as integer seconds since the epoch; RFC3339 formatting
    // happens in Rust (via chrono, a runtime dependency) so this script needs no
    // date-formatting crate and stays portable across platforms.
    let epoch = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
        });
    if let Some(epoch) = epoch {
        println!("cargo:rustc-env=OMNI_DEV_BUILD_EPOCH={epoch}");
    }

    // Rerun triggers: recompute when HEAD or the checked-out branch ref moves.
    // Paths are resolved via `git rev-parse --git-path` so linked worktrees —
    // where `.git` is a file pointing elsewhere, not a directory — work too.
    // (Trade-off: the dirty flag can lag until the next commit/branch change,
    // since uncommitted edits have no file here to watch. Acceptable per #1374.)
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(ref_path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={ref_path}");
        }
    }
    if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"]) {
        println!("cargo:rerun-if-changed={packed}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
