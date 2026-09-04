//! Relocates a Claude Code session's on-disk transcript storage from one
//! worktree's project scope to another.
//!
//! The daemon-independent filesystem logic behind the worktrees UI's
//! "Move/Copy Claude session here" action (issue #1585 Phase 2), ported from
//! the VS Code companion's `claudeSessions.ts`/`moveSessionCommand.ts`
//! (#1295). Pure filesystem manipulation under `~/.claude/projects/` — no
//! daemon op, no network call, nothing here talks to the daemon or the
//! worktrees/sessions registries.
//!
//! Both the source and destination project folders live under the same
//! `~/.claude/projects/` root, so a relocation is always same-filesystem —
//! `fs::rename` is atomic (no cross-device fallback needed) for the move
//! case.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::watcher::projects_dir;

/// How recently a transcript must have been written to be treated as
/// possibly-live and refused. Best-effort — an idle-but-open session may not
/// write for minutes, so this only catches an actively-streaming one; the
/// caller's own confirmation step is the real safety net.
const LIVE_THRESHOLD: Duration = Duration::from_secs(10);

/// Encodes an absolute directory path the way Claude Code names its
/// per-project storage folder under `~/.claude/projects/`: every `/` and `.`
/// becomes `-`. Lossy and one-way — only ever used to encode.
pub(crate) fn encode_project_path(abs_path: &Path) -> String {
    abs_path
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// The encoded per-project session folder for an absolute worktree/cwd path.
/// `None` only when [`projects_dir`] itself can't be resolved (no home
/// directory and no override).
pub(crate) fn project_dir_for(abs_path: &Path) -> Option<PathBuf> {
    Some(projects_dir()?.join(encode_project_path(abs_path)))
}

/// Whether a relocation moves the artifacts or copies them (a fork, leaving
/// the original in place under the same session id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelocationMode {
    Move,
    Copy,
}

/// One filesystem operation in a relocation plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelocationOp {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
    pub(crate) is_dir: bool,
}

/// The ordered artifact operations to relocate one session.
#[derive(Debug, Clone)]
pub(crate) struct RelocationPlan {
    /// Not read by `execute_relocation` (which only needs `ops`/`mode`) —
    /// kept for callers building a summary/log line from the plan itself.
    #[allow(dead_code)]
    pub(crate) session_id: String,
    pub(crate) mode: RelocationMode,
    pub(crate) ops: Vec<RelocationOp>,
}

/// One session discovered in a source project folder.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    /// The session id — the `.jsonl` basename, and the sidecar dir name.
    pub(crate) id: String,
    /// Not read by the relocation flow itself (`plan_relocation` derives its
    /// own paths from `id`/`src_dir`); kept for a future picker preview
    /// (reading the transcript's head for a human-readable label, ported
    /// from `moveSessionCommand.ts::readPreview` — deferred to Phase 5
    /// polish per the plan, see `actions.rs`'s module doc).
    #[allow(dead_code)]
    pub(crate) jsonl_path: PathBuf,
    pub(crate) modified: SystemTime,
    /// Whether an `<id>/` sidecar dir (subagent/tool-result overflow)
    /// accompanies the transcript.
    pub(crate) has_sidecar: bool,
}

/// Builds the ordered filesystem operations to relocate one session from
/// `src_dir` to `dest_dir`. The transcript `<id>.jsonl` is always included;
/// the `<id>/` sidecar dir only when `has_sidecar`. Ordered
/// transcript-first, so a partial failure still leaves a moved, resumable
/// transcript rather than an orphaned sidecar.
pub(crate) fn plan_relocation(
    session_id: &str,
    src_dir: &Path,
    dest_dir: &Path,
    has_sidecar: bool,
    mode: RelocationMode,
) -> RelocationPlan {
    let mut ops = vec![RelocationOp {
        from: src_dir.join(format!("{session_id}.jsonl")),
        to: dest_dir.join(format!("{session_id}.jsonl")),
        is_dir: false,
    }];
    if has_sidecar {
        ops.push(RelocationOp {
            from: src_dir.join(session_id),
            to: dest_dir.join(session_id),
            is_dir: true,
        });
    }
    RelocationPlan {
        session_id: session_id.to_string(),
        mode,
        ops,
    }
}

/// Lists the sessions in a source project folder, newest first. A missing
/// folder (no session ever ran under this project scope) yields an empty
/// list, not an error.
pub(crate) fn enumerate_sessions(src_dir: &Path) -> Result<Vec<SessionInfo>> {
    let entries = match fs::read_dir(src_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", src_dir.display())),
    };

    let mut sidecar_dirs: HashSet<String> = HashSet::new();
    let mut transcripts: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", src_dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            sidecar_dirs.insert(name.into_owned());
        } else if file_type.is_file() {
            if let Some(id) = name.strip_suffix(".jsonl") {
                transcripts.push((id.to_string(), entry.path()));
            }
        }
    }

    let mut sessions = Vec::new();
    for (id, jsonl_path) in transcripts {
        let Ok(metadata) = fs::metadata(&jsonl_path) else {
            continue; // raced with a delete between readdir and stat
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        sessions.push(SessionInfo {
            has_sidecar: sidecar_dirs.contains(&id),
            id,
            jsonl_path,
            modified,
        });
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(sessions)
}

/// Whether `modified` is recent enough that the session may still be live —
/// the guard against moving a transcript out from under a running session.
pub(crate) fn is_likely_live(modified: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(modified) {
        Ok(elapsed) => elapsed <= LIVE_THRESHOLD,
        Err(_) => true, // a future mtime (clock skew) counts as recent
    }
}

/// The destination artifact that would be clobbered, or `None` when clear —
/// a relocation must never overwrite an existing session of the same id.
pub(crate) fn destination_collision(session: &SessionInfo, dest_dir: &Path) -> Option<String> {
    let jsonl = dest_dir.join(format!("{}.jsonl", session.id));
    if jsonl.exists() {
        return Some(format!("{}.jsonl", session.id));
    }
    if session.has_sidecar {
        let dir = dest_dir.join(&session.id);
        if dir.exists() {
            return Some(format!("{}/", session.id));
        }
    }
    None
}

/// Executes a relocation plan: creates `dest_dir` if needed, then applies
/// each op in order (transcript first — see [`plan_relocation`]) via
/// `fs::rename` (move) or a recursive copy (fork).
pub(crate) fn execute_relocation(plan: &RelocationPlan, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;
    for op in &plan.ops {
        match plan.mode {
            RelocationMode::Move => {
                fs::rename(&op.from, &op.to).with_context(|| {
                    format!(
                        "failed to move {} to {}",
                        op.from.display(),
                        op.to.display()
                    )
                })?;
            }
            RelocationMode::Copy if op.is_dir => {
                copy_dir_recursive(&op.from, &op.to).with_context(|| {
                    format!(
                        "failed to copy {} to {}",
                        op.from.display(),
                        op.to.display()
                    )
                })?;
            }
            RelocationMode::Copy => {
                fs::copy(&op.from, &op.to).with_context(|| {
                    format!(
                        "failed to copy {} to {}",
                        op.from.display(),
                        op.to.display()
                    )
                })?;
            }
        }
    }
    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn encode_project_path_replaces_slashes_and_dots() {
        assert_eq!(
            encode_project_path(Path::new("/Users/x/wrk/omni-dev")),
            "-Users-x-wrk-omni-dev"
        );
        assert_eq!(
            encode_project_path(Path::new("/Users/x/Downloads/Dot.dot")),
            "-Users-x-Downloads-Dot-dot"
        );
        assert_eq!(encode_project_path(Path::new("/a/.work")), "-a--work");
    }

    #[test]
    fn plan_relocation_is_transcript_first_and_includes_sidecar_only_when_present() {
        let plan = plan_relocation(
            "abc123",
            Path::new("/src"),
            Path::new("/dest"),
            true,
            RelocationMode::Move,
        );
        assert_eq!(plan.ops.len(), 2);
        assert_eq!(plan.ops[0].from, PathBuf::from("/src/abc123.jsonl"));
        assert!(!plan.ops[0].is_dir);
        assert_eq!(plan.ops[1].from, PathBuf::from("/src/abc123"));
        assert!(plan.ops[1].is_dir);

        let plan_no_sidecar = plan_relocation(
            "abc123",
            Path::new("/src"),
            Path::new("/dest"),
            false,
            RelocationMode::Copy,
        );
        assert_eq!(plan_no_sidecar.ops.len(), 1);
    }

    #[test]
    fn enumerate_sessions_returns_empty_for_a_missing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(enumerate_sessions(&missing).unwrap().is_empty());
    }

    #[test]
    fn enumerate_sessions_finds_transcripts_newest_first_and_detects_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("older.jsonl");
        let newer = dir.path().join("newer.jsonl");
        fs::write(&older, "{}").unwrap();
        // Ensure a real mtime gap regardless of filesystem timestamp
        // resolution, then write the newer file second.
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&newer, "{}").unwrap();
        fs::create_dir(dir.path().join("newer")).unwrap();

        let sessions = enumerate_sessions(dir.path()).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "newer");
        assert!(sessions[0].has_sidecar);
        assert_eq!(sessions[1].id, "older");
        assert!(!sessions[1].has_sidecar);
    }

    #[test]
    fn is_likely_live_window_boundaries() {
        let now = SystemTime::now();
        assert!(is_likely_live(now, now));
        assert!(is_likely_live(now - Duration::from_secs(5), now));
        assert!(!is_likely_live(now - Duration::from_secs(60), now));
        // A future mtime (clock skew) counts as recent.
        assert!(is_likely_live(now + Duration::from_secs(30), now));
    }

    #[test]
    fn destination_collision_detects_jsonl_and_sidecar_clashes() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionInfo {
            id: "abc".to_string(),
            jsonl_path: PathBuf::from("/src/abc.jsonl"),
            modified: UNIX_EPOCH,
            has_sidecar: true,
        };
        assert_eq!(destination_collision(&session, dir.path()), None);

        fs::write(dir.path().join("abc.jsonl"), "{}").unwrap();
        assert_eq!(
            destination_collision(&session, dir.path()),
            Some("abc.jsonl".to_string())
        );

        fs::remove_file(dir.path().join("abc.jsonl")).unwrap();
        fs::create_dir(dir.path().join("abc")).unwrap();
        assert_eq!(
            destination_collision(&session, dir.path()),
            Some("abc/".to_string())
        );
    }

    #[test]
    fn execute_relocation_move_relocates_transcript_and_sidecar() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("src");
        let dest = root.path().join("dest");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("abc.jsonl"), "transcript").unwrap();
        fs::create_dir(src.join("abc")).unwrap();
        fs::write(src.join("abc").join("tool.json"), "sidecar").unwrap();

        let plan = plan_relocation("abc", &src, &dest, true, RelocationMode::Move);
        execute_relocation(&plan, &dest).unwrap();

        assert!(!src.join("abc.jsonl").exists());
        assert!(!src.join("abc").exists());
        assert!(dest.join("abc.jsonl").exists());
        assert!(dest.join("abc").join("tool.json").exists());
    }

    #[test]
    fn execute_relocation_copy_leaves_the_source_in_place() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("src");
        let dest = root.path().join("dest");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("abc.jsonl"), "transcript").unwrap();

        let plan = plan_relocation("abc", &src, &dest, false, RelocationMode::Copy);
        execute_relocation(&plan, &dest).unwrap();

        assert!(src.join("abc.jsonl").exists(), "source must remain");
        assert!(dest.join("abc.jsonl").exists());
    }

    #[test]
    fn execute_relocation_move_stops_after_the_transcript_when_the_sidecar_move_fails() {
        // Transcript-first ordering (see plan_relocation's doc comment): if the
        // sidecar move fails, the transcript must already have landed at the
        // destination — never orphaned mid-plan.
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("src");
        let dest = root.path().join("dest");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("abc.jsonl"), "transcript").unwrap();
        // No sidecar directory created at all, so the second op's rename fails.
        let plan = plan_relocation("abc", &src, &dest, true, RelocationMode::Move);

        assert!(execute_relocation(&plan, &dest).is_err());
        assert!(
            dest.join("abc.jsonl").exists(),
            "transcript must have moved before the sidecar op failed"
        );
        assert!(!src.join("abc.jsonl").exists());
    }
}
