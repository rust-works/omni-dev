//! `omni-dev ai claude history sync` — exports Claude Code conversation
//! history to a target directory as one `.jsonl` per chat.
//!
//! See the issue and the module-level docs in [`super`] for the design rationale
//! (behavioural transcript vs faithful archive). This file implements only the
//! algorithm; rendering lives in [`super`].

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use serde::Serialize;

use super::common::{decode_slug, default_source_root, is_inside, parse_since, OutputFormat};

const JSONL_EXT: &str = "jsonl";

/// Exports Claude Code conversation history to a target directory.
#[derive(Parser, Debug)]
pub struct SyncCommand {
    /// Target directory for the export. Created if it does not exist.
    #[arg(long, value_name = "PATH")]
    pub target: PathBuf,

    /// Override source root. Defaults to `~/.claude/projects`.
    #[arg(long, value_name = "PATH")]
    pub source: Option<PathBuf>,

    /// Restrict sync to one project. Accepts the encoded directory name
    /// (e.g. `-Users-jky-tmp`) or a decoded cwd path (matched after decoding).
    // `allow_hyphen_values` lets the leading `-` in encoded slug names pass
    // through clap rather than being parsed as an unknown flag.
    #[arg(long, value_name = "NAME_OR_PATH", allow_hyphen_values = true)]
    pub project: Option<String>,

    /// Only sync sessions whose source mtime is at or after this point.
    /// Accepts a relative duration (`30s`, `5m`, `2h`, `7d`, `4w`) or an
    /// RFC 3339 timestamp.
    #[arg(long, value_name = "DURATION_OR_DATE")]
    pub since: Option<String>,

    /// Delete target files for sessions no longer present in the source.
    /// Only files matching `<slug>/<uuid>.jsonl` are eligible for deletion;
    /// anything else inside the target is preserved regardless.
    #[arg(long)]
    pub prune: bool,

    /// Preview changes without touching the target.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

impl SyncCommand {
    /// Executes the sync.
    pub fn execute(self) -> Result<()> {
        let report = run(SyncOptions {
            target: &self.target,
            source: self.source.as_deref(),
            project: self.project.as_deref(),
            since: self.since.as_deref(),
            prune: self.prune,
            dry_run: self.dry_run,
            now: Utc::now(),
        })?;
        super::print_report(&report, self.dry_run, self.format)?;
        if !report.errors.is_empty() {
            anyhow::bail!(
                "{} session(s) failed to sync; see errors above",
                report.errors.len()
            );
        }
        Ok(())
    }
}

/// Options for [`run`]. Public for tests in sibling modules.
pub struct SyncOptions<'a> {
    pub target: &'a Path,
    pub source: Option<&'a Path>,
    pub project: Option<&'a str>,
    pub since: Option<&'a str>,
    pub prune: bool,
    pub dry_run: bool,
    pub now: DateTime<Utc>,
}

/// Outcome of a sync run.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub actions: Vec<SyncAction>,
    pub errors: Vec<SyncError>,
}

/// One unit of work the sync performed (or, in dry-run mode, would perform).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncAction {
    Created {
        project: String,
        session: String,
        target: PathBuf,
        bytes: u64,
    },
    Updated {
        project: String,
        session: String,
        target: PathBuf,
        bytes: u64,
    },
    Skipped {
        project: String,
        session: String,
        target: PathBuf,
        reason: SkipReason,
    },
    Pruned {
        project: String,
        session: String,
        target: PathBuf,
    },
}

/// Why a session was skipped during planning.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Unchanged,
    FilteredBySince,
    FilteredByProject,
}

/// An error encountered while processing one session. Other sessions still run.
#[derive(Debug, Clone, Serialize)]
pub struct SyncError {
    pub project: String,
    pub session: String,
    pub reason: String,
}

/// Performs the sync. Library entry point — does no I/O on stdout itself.
pub fn run(opts: SyncOptions<'_>) -> Result<SyncReport> {
    let source_root_owned;
    let source_root = if let Some(p) = opts.source {
        p
    } else {
        source_root_owned = default_source_root()?;
        source_root_owned.as_path()
    };
    if !source_root.exists() {
        anyhow::bail!(
            "source directory does not exist: {} (set --source or create the directory)",
            source_root.display()
        );
    }
    if !source_root.is_dir() {
        anyhow::bail!("source path is not a directory: {}", source_root.display());
    }

    if is_inside(opts.target, source_root) {
        anyhow::bail!(
            "refusing to sync: target {} is inside source {}",
            opts.target.display(),
            source_root.display()
        );
    }

    if !opts.dry_run {
        fs::create_dir_all(opts.target)
            .with_context(|| format!("Failed to create target {}", opts.target.display()))?;
    }

    let cutoff = match opts.since {
        Some(spec) => Some(parse_since(spec, opts.now)?),
        None => None,
    };

    let mut report = SyncReport::default();

    let project_filter = opts.project;

    let project_dirs = list_project_dirs(source_root)?;
    for project_dir in project_dirs {
        let slug = match project_dir.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !project_matches(project_filter, &slug) {
            // Record skip per session in this project so the user sees the filter took effect.
            for session in list_sessions(&project_dir)? {
                let target_path = opts
                    .target
                    .join(&slug)
                    .join(format!("{}.{}", session.uuid, JSONL_EXT));
                report.actions.push(SyncAction::Skipped {
                    project: slug.clone(),
                    session: session.uuid,
                    target: target_path,
                    reason: SkipReason::FilteredByProject,
                });
            }
            continue;
        }

        for session in list_sessions(&project_dir)? {
            let target_dir = opts.target.join(&slug);
            let target_path = target_dir.join(format!("{}.{}", session.uuid, JSONL_EXT));

            if let Some(cutoff) = cutoff {
                let mtime: DateTime<Utc> = session.mtime.into();
                if mtime < cutoff {
                    report.actions.push(SyncAction::Skipped {
                        project: slug.clone(),
                        session: session.uuid,
                        target: target_path,
                        reason: SkipReason::FilteredBySince,
                    });
                    continue;
                }
            }

            let action = match plan_session(&session, &target_path) {
                Ok(Plan::Skip) => Ok(SyncAction::Skipped {
                    project: slug.clone(),
                    session: session.uuid.clone(),
                    target: target_path.clone(),
                    reason: SkipReason::Unchanged,
                }),
                Ok(Plan::Create) => copy_session(&session, &target_dir, &target_path, opts.dry_run)
                    .map(|bytes| SyncAction::Created {
                        project: slug.clone(),
                        session: session.uuid.clone(),
                        target: target_path.clone(),
                        bytes,
                    }),
                Ok(Plan::Update) => copy_session(&session, &target_dir, &target_path, opts.dry_run)
                    .map(|bytes| SyncAction::Updated {
                        project: slug.clone(),
                        session: session.uuid.clone(),
                        target: target_path.clone(),
                        bytes,
                    }),
                Err(e) => Err(e),
            };
            match action {
                Ok(a) => report.actions.push(a),
                Err(e) => report.errors.push(SyncError {
                    project: slug.clone(),
                    session: session.uuid.clone(),
                    reason: format!("{e:#}"),
                }),
            }
        }
    }

    if opts.prune {
        prune_target(opts.target, source_root, opts.dry_run, &mut report)?;
    }

    Ok(report)
}

/// Returns true if the slug should be processed (not filtered out). When
/// `false`, the caller emits `Skipped(FilteredByProject)` for the project's
/// sessions so the user sees the filter took effect.
fn project_matches(filter: Option<&str>, slug: &str) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    filter == slug || decode_slug(slug) == filter
}

#[derive(Debug)]
struct Session {
    uuid: String,
    src_path: PathBuf,
    size: u64,
    mtime: SystemTime,
}

fn list_project_dirs(source_root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let read = fs::read_dir(source_root)
        .with_context(|| format!("Failed to read source {}", source_root.display()))?;
    for entry in read {
        let entry =
            entry.with_context(|| format!("Failed to read entry in {}", source_root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        out.push(path);
    }
    out.sort();
    Ok(out)
}

fn list_sessions(project_dir: &Path) -> Result<Vec<Session>> {
    let mut out = Vec::new();
    let read = fs::read_dir(project_dir)
        .with_context(|| format!("Failed to read project {}", project_dir.display()))?;
    for entry in read {
        let entry =
            entry.with_context(|| format!("Failed to read entry in {}", project_dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some(JSONL_EXT) {
            continue;
        }
        let Some(uuid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let metadata = entry
            .metadata()
            .with_context(|| format!("Failed to stat {}", path.display()))?;
        let mtime = metadata
            .modified()
            .with_context(|| format!("Failed to read mtime of {}", path.display()))?;
        out.push(Session {
            uuid,
            src_path: path,
            size: metadata.len(),
            mtime,
        });
    }
    out.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    Ok(out)
}

#[derive(Debug)]
enum Plan {
    Create,
    Update,
    Skip,
}

fn plan_session(session: &Session, target_path: &Path) -> Result<Plan> {
    match fs::metadata(target_path) {
        Ok(meta) => {
            let same_size = meta.len() == session.size;
            let same_mtime = meta.modified().ok().is_some_and(|t| t == session.mtime);
            if same_size && same_mtime {
                Ok(Plan::Skip)
            } else {
                Ok(Plan::Update)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Plan::Create),
        Err(e) => Err(e).with_context(|| format!("Failed to stat {}", target_path.display())),
    }
}

/// Copies a session from `session.src_path` to `target_path` using a
/// snapshot-EOF read of `session.size` bytes — sessions still being appended
/// to upstream produce a valid prefix.
///
/// In dry-run mode this performs no I/O on the target and returns the source
/// length. The "bytes" value reported in the action is the planned-to-copy
/// length, not what is on disk after a dry run.
fn copy_session(
    session: &Session,
    target_dir: &Path,
    target_path: &Path,
    dry_run: bool,
) -> Result<u64> {
    if dry_run {
        return Ok(session.size);
    }

    fs::create_dir_all(target_dir)
        .with_context(|| format!("Failed to create {}", target_dir.display()))?;

    let mut src = BufReader::new(
        File::open(&session.src_path)
            .with_context(|| format!("Failed to open {}", session.src_path.display()))?,
    );

    // Stage to a sibling tempfile so a partially-copied target never overwrites
    // a previously-good one. The rename below is atomic on the same filesystem.
    let mut tmp = tempfile::NamedTempFile::new_in(target_dir)
        .with_context(|| format!("Failed to create temp in {}", target_dir.display()))?;
    let copied = {
        let mut writer = BufWriter::new(tmp.as_file_mut());
        let mut limited = (&mut src).take(session.size);
        let copied = io::copy(&mut limited, &mut writer)
            .with_context(|| format!("Failed to copy {}", session.src_path.display()))?;
        writer
            .flush()
            .with_context(|| format!("Failed to flush {}", target_path.display()))?;
        copied
    };

    let persisted = tmp
        .persist(target_path)
        .map_err(|e| e.error)
        .with_context(|| format!("Failed to publish {}", target_path.display()))?;
    drop(persisted);

    set_mtime(target_path, session.mtime)
        .with_context(|| format!("Failed to set mtime on {}", target_path.display()))?;

    Ok(copied)
}

fn set_mtime(path: &Path, mtime: SystemTime) -> io::Result<()> {
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_modified(mtime)
}

fn prune_target(
    target_root: &Path,
    source_root: &Path,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    let target_entries = match fs::read_dir(target_root) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read target {}", target_root.display()));
        }
    };

    let mut slug_dirs: Vec<PathBuf> = Vec::new();
    for entry in target_entries {
        let entry =
            entry.with_context(|| format!("Failed to read entry in {}", target_root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        slug_dirs.push(path);
    }
    slug_dirs.sort();

    for slug_dir in slug_dirs {
        let slug = match slug_dir.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let source_slug_dir = source_root.join(&slug);

        let read = fs::read_dir(&slug_dir)
            .with_context(|| format!("Failed to read {}", slug_dir.display()))?;
        for entry in read {
            let entry =
                entry.with_context(|| format!("Failed to read entry in {}", slug_dir.display()))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some(JSONL_EXT) {
                continue;
            }
            let Some(uuid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let source_file = source_slug_dir.join(format!("{uuid}.{JSONL_EXT}"));
            if source_file.exists() {
                continue;
            }
            if !dry_run {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to delete {}", path.display()))?;
            }
            report.actions.push(SyncAction::Pruned {
                project: slug.clone(),
                session: uuid,
                target: path,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs::File as StdFile;
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    /// `TempDir::path()` returns a path with macOS's `/var/folders/...` form,
    /// which canonicalises to `/private/var/folders/...`. Tests that compare
    /// derived paths against `tmp.path()` will fail; canonicalising once at
    /// the start sidesteps the entire class of platform aliasing issues.
    fn tempdir() -> TempDir {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&root).ok();
        TempDir::new_in(&root).unwrap()
    }

    /// Source directory mock: writes one session jsonl per `(slug, uuid)`
    /// pair with the given content. Returns the source root.
    struct SourceBuilder {
        root: PathBuf,
    }

    impl SourceBuilder {
        fn new(root: PathBuf) -> Self {
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn add(&self, slug: &str, uuid: &str, content: &str) -> PathBuf {
            let dir = self.root.join(slug);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{uuid}.jsonl"));
            std::fs::write(&path, content).unwrap();
            path
        }

        fn append(&self, slug: &str, uuid: &str, more: &str) {
            let path = self.root.join(slug).join(format!("{uuid}.jsonl"));
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(more.as_bytes()).unwrap();
            // Bump mtime explicitly so the test isn't sensitive to filesystem
            // mtime granularity (HFS+ in particular uses 1s resolution).
            let new_mtime = SystemTime::now() + Duration::from_secs(2);
            StdFile::open(&path)
                .unwrap()
                .set_modified(new_mtime)
                .unwrap();
        }
    }

    fn run_default(target: &Path, source: &Path) -> Result<SyncReport> {
        run(SyncOptions {
            target,
            source: Some(source),
            project: None,
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
    }

    fn count_created(r: &SyncReport) -> usize {
        r.actions
            .iter()
            .filter(|a| matches!(a, SyncAction::Created { .. }))
            .count()
    }

    fn count_updated(r: &SyncReport) -> usize {
        r.actions
            .iter()
            .filter(|a| matches!(a, SyncAction::Updated { .. }))
            .count()
    }

    fn count_skipped(r: &SyncReport) -> usize {
        r.actions
            .iter()
            .filter(|a| matches!(a, SyncAction::Skipped { .. }))
            .count()
    }

    fn count_pruned(r: &SyncReport) -> usize {
        r.actions
            .iter()
            .filter(|a| matches!(a, SyncAction::Pruned { .. }))
            .count()
    }

    fn count_filtered_by(r: &SyncReport, want: &SkipReason) -> usize {
        r.actions
            .iter()
            .filter(|a| matches!(a, SyncAction::Skipped { reason, .. } if reason == want))
            .count()
    }

    #[test]
    fn fresh_sync_creates_files_with_matching_bytes() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("-Users-jky-foo", "uuid-1", "{\"x\":1}\n");
        sb.add("-Users-jky-foo", "uuid-2", "{\"x\":2}\n");
        sb.add("-Users-jky-bar", "uuid-3", "{\"x\":3}\n");

        let report = run_default(tgt.path(), src.path()).unwrap();
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert_eq!(count_created(&report), 3);

        for (slug, uuid, body) in [
            ("-Users-jky-foo", "uuid-1", "{\"x\":1}\n"),
            ("-Users-jky-foo", "uuid-2", "{\"x\":2}\n"),
            ("-Users-jky-bar", "uuid-3", "{\"x\":3}\n"),
        ] {
            let target_path = tgt.path().join(slug).join(format!("{uuid}.jsonl"));
            assert_eq!(std::fs::read_to_string(&target_path).unwrap(), body);
        }
    }

    #[test]
    fn second_run_is_noop() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "line\n");

        run_default(tgt.path(), src.path()).unwrap();
        let report = run_default(tgt.path(), src.path()).unwrap();
        assert!(matches!(
            report.actions[0],
            SyncAction::Skipped {
                reason: SkipReason::Unchanged,
                ..
            }
        ));
        assert_eq!(report.actions.len(), 1);
    }

    #[test]
    fn modified_source_triggers_update() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "line\n");
        sb.add("slug", "u2", "stable\n");

        run_default(tgt.path(), src.path()).unwrap();
        sb.append("slug", "u1", "more\n");

        let report = run_default(tgt.path(), src.path()).unwrap();
        assert_eq!(count_updated(&report), 1, "actions: {:?}", report.actions);
        assert_eq!(count_skipped(&report), 1);

        let body = std::fs::read_to_string(tgt.path().join("slug").join("u1.jsonl")).unwrap();
        assert_eq!(body, "line\nmore\n");
    }

    #[test]
    fn new_chat_added_between_runs() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();

        sb.add("slug", "u2", "b\n");
        let report = run_default(tgt.path(), src.path()).unwrap();
        assert_eq!(count_created(&report), 1);
    }

    #[test]
    fn prune_deletes_only_matching_files_when_requested() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        sb.add("slug", "u2", "b\n");
        run_default(tgt.path(), src.path()).unwrap();

        // Drop a stray file that does not match `<slug>/<uuid>.jsonl` shape.
        let stray_top = tgt.path().join("README.md");
        std::fs::write(&stray_top, "keep me").unwrap();
        let stray_in_slug = tgt.path().join("slug").join("notes.txt");
        std::fs::write(&stray_in_slug, "keep me too").unwrap();

        // Remove u1 from source and run with --prune.
        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: None,
            since: None,
            prune: true,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap();

        assert_eq!(count_pruned(&report), 1, "actions: {:?}", report.actions);
        assert!(!tgt.path().join("slug").join("u1.jsonl").exists());
        assert!(tgt.path().join("slug").join("u2.jsonl").exists());

        // Stray files survive prune.
        assert!(stray_top.exists());
        assert!(stray_in_slug.exists());
    }

    #[test]
    fn no_prune_leaves_orphans_alone() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();

        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let report = run_default(tgt.path(), src.path()).unwrap();
        // No source file -> no action; orphan target file is untouched.
        assert!(report
            .actions
            .iter()
            .all(|a| !matches!(a, SyncAction::Pruned { .. })));
        assert!(tgt.path().join("slug").join("u1.jsonl").exists());
    }

    #[test]
    fn dry_run_does_not_touch_target() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "abc\n");

        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: None,
            since: None,
            prune: false,
            dry_run: true,
            now: chrono::Utc::now(),
        })
        .unwrap();
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Created { .. })));
        // Target slug subdir was never created.
        assert!(!tgt.path().join("slug").exists());
    }

    #[test]
    fn project_filter_matches_encoded_slug() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("-Users-jky-foo", "u1", "f\n");
        sb.add("-Users-jky-bar", "u2", "b\n");

        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: Some("-Users-jky-foo"),
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap();
        assert_eq!(count_created(&report), 1);
        assert_eq!(
            count_filtered_by(&report, &SkipReason::FilteredByProject),
            1
        );
    }

    #[test]
    fn project_filter_matches_decoded_path() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("-Users-jky-foo", "u1", "f\n");
        sb.add("-Users-jky-bar", "u2", "b\n");

        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: Some("/Users/jky/foo"),
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap();
        assert_eq!(count_created(&report), 1);
    }

    #[test]
    fn project_filter_no_match_skips_everything() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug-a", "u1", "a\n");
        sb.add("slug-b", "u2", "b\n");

        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: Some("nonexistent"),
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap();
        assert!(report.actions.iter().all(|a| matches!(
            a,
            SyncAction::Skipped {
                reason: SkipReason::FilteredByProject,
                ..
            }
        )));
    }

    #[test]
    fn since_filters_old_sessions() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        let old_path = sb.add("slug", "old", "old\n");
        sb.add("slug", "new", "new\n");

        // Backdate "old" session 30 days into the past.
        let old_mtime = SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
        StdFile::open(&old_path)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();

        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: None,
            since: Some("1d"),
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap();

        assert_eq!(count_created(&report), 1);
        assert_eq!(count_filtered_by(&report, &SkipReason::FilteredBySince), 1);
    }

    #[test]
    fn target_inside_source_is_refused() {
        let src = tempdir();
        let tgt_inside = src.path().join("inside");
        std::fs::create_dir_all(&tgt_inside).unwrap();
        let err = run(SyncOptions {
            target: &tgt_inside,
            source: Some(src.path()),
            project: None,
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("inside"), "unexpected: {msg}");
    }

    #[test]
    fn missing_source_errors_clearly() {
        let tgt = tempdir();
        let nope = tgt.path().join("does-not-exist");
        let err = run(SyncOptions {
            target: tgt.path(),
            source: Some(&nope),
            project: None,
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"));
    }

    #[test]
    fn source_path_must_be_directory() {
        let src = tempdir();
        let tgt = tempdir();
        let file = src.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let err = run(SyncOptions {
            target: tgt.path(),
            source: Some(&file),
            project: None,
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("not a directory"));
    }

    #[test]
    fn snapshot_eof_copies_only_initial_length() {
        // Simulate a chat that grows mid-sync: list_sessions captures the
        // initial size; copy_session must not exceed it.
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        let path = sb.add("slug", "u1", "first-half\n");

        // Pre-compute size, then append more bytes BEFORE running sync.
        let snapshot_len = std::fs::metadata(&path).unwrap().len();
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"second-half-appended-after-snapshot\n")
                .unwrap();
        }
        // Fake out the planner: write a Session manually that pins the old size.
        let session = Session {
            uuid: "u1".to_string(),
            src_path: path.clone(),
            size: snapshot_len,
            mtime: std::fs::metadata(&path).unwrap().modified().unwrap(),
        };
        let target_dir = tgt.path().join("slug");
        let target_path = target_dir.join("u1.jsonl");
        let copied = copy_session(&session, &target_dir, &target_path, false).unwrap();
        assert_eq!(copied, snapshot_len);
        let body = std::fs::read_to_string(&target_path).unwrap();
        assert_eq!(body, "first-half\n");
    }

    #[test]
    fn mtime_is_preserved_on_target() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        let path = sb.add("slug", "u1", "data\n");
        // Set a very specific mtime so the comparison is unambiguous.
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        StdFile::open(&path).unwrap().set_modified(ts).unwrap();

        run_default(tgt.path(), src.path()).unwrap();
        let target_path = tgt.path().join("slug").join("u1.jsonl");
        let target_mtime = std::fs::metadata(&target_path).unwrap().modified().unwrap();
        let target_secs = target_mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(target_secs, 1_700_000_000);
    }

    #[test]
    fn ignores_non_jsonl_and_subdirs_under_project() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        // Add a non-jsonl file and a sub-agent-style subdirectory; sync ignores both.
        let project_dir = src.path().join("slug");
        std::fs::write(project_dir.join("sessions-index.json"), "{}").unwrap();
        std::fs::create_dir_all(project_dir.join("u1").join("subagents")).unwrap();
        std::fs::write(
            project_dir
                .join("u1")
                .join("subagents")
                .join("agent-x.jsonl"),
            "noise\n",
        )
        .unwrap();

        let report = run_default(tgt.path(), src.path()).unwrap();
        assert_eq!(count_created(&report), 1, "actions: {:?}", report.actions);

        // Target contains only the one file, not the noise.
        let target_slug = tgt.path().join("slug");
        let entries: Vec<_> = std::fs::read_dir(&target_slug)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "u1.jsonl");
    }

    #[test]
    fn execute_via_clap_round_trip_works() {
        // Drives the full SyncCommand::execute path so the clap derive and
        // print_report code aren't dead.
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");

        let cmd = SyncCommand::try_parse_from([
            "history-sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            tgt.path().to_str().unwrap(),
            "--format",
            "yaml",
        ])
        .unwrap();
        cmd.execute().unwrap();
        assert!(tgt.path().join("slug").join("u1.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn execute_returns_error_when_target_is_unwritable() {
        // Force copy_session to fail by making the target slug directory
        // read-only. Sync proceeds for other sessions, then bail()s because
        // the report carries errors.
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");

        // Pre-create the slug subdir as read-only so the temp-file write fails.
        let slug_dir = tgt.path().join("slug");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let cmd = SyncCommand::try_parse_from([
            "history-sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            tgt.path().to_str().unwrap(),
        ])
        .unwrap();
        let err = cmd.execute().unwrap_err();
        assert!(format!("{err:#}").contains("session(s) failed"));

        // Restore perms so TempDir can drop cleanly.
        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn target_root_is_canonicalised_for_inside_check_via_macos_alias() {
        // On macOS /tmp aliases /private/tmp — exercise the canonicalisation
        // walk-up. We use the actual tempdir whose parent canonicalises.
        let src = tempdir();
        let tgt_outside = tempdir();
        // Fresh sync should succeed when target is genuinely outside source.
        let report = run_default(tgt_outside.path(), src.path()).unwrap();
        assert!(report.errors.is_empty());
    }

    #[test]
    fn default_source_root_returns_a_path_under_home() {
        let p = super::super::common::default_source_root().unwrap();
        // Constructed from $HOME, so must be absolute and end in `.claude/projects`.
        assert!(p.is_absolute());
        assert!(p.ends_with(".claude/projects"));
    }

    #[test]
    fn dispatch_via_history_command_clap() {
        // Drive HistoryCommand::execute through clap (covers mod.rs dispatch).
        use super::super::HistoryCommand;
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        let cmd = HistoryCommand::try_parse_from([
            "history",
            "sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            tgt.path().to_str().unwrap(),
        ])
        .unwrap();
        cmd.execute().unwrap();
        assert!(tgt.path().join("slug").join("u1.jsonl").exists());
    }

    #[test]
    fn prune_handles_missing_target_dir_silently() {
        // --prune on a target that does not exist (e.g. dry-run) returns Ok
        // and does nothing. Construct a SyncReport in-place to reach the
        // NotFound branch of prune_target.
        let src = tempdir();
        let tgt_root = tempdir();
        let nonexistent = tgt_root.path().join("not-yet-here");
        let mut report = SyncReport::default();
        // prune_target is private; reach it via a real run that we know
        // creates the target via mkdir, then delete it before invoking prune
        // again with the same options.
        prune_target(&nonexistent, src.path(), false, &mut report).unwrap();
        assert!(report.actions.is_empty());
    }

    #[test]
    fn prune_skips_non_jsonl_and_non_files_inside_target_slug() {
        // Drops a directory and a non-jsonl file inside the target slug, then
        // verifies --prune leaves them alone (covers the continue branches in
        // prune_target).
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();

        let slug_dir = tgt.path().join("slug");
        std::fs::create_dir_all(slug_dir.join("subdir")).unwrap();
        std::fs::write(slug_dir.join("notes.txt"), "x").unwrap();

        // Remove u1 from source to make --prune trigger.
        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: None,
            since: None,
            prune: true,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap();

        // Pruned exactly u1; subdir and notes.txt survive.
        assert_eq!(count_pruned(&report), 1);
        assert!(slug_dir.join("subdir").exists());
        assert!(slug_dir.join("notes.txt").exists());
    }

    #[test]
    fn prune_skips_non_directory_entries_at_target_root() {
        // Target root with a stray file at top level alongside slug subdirs.
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();
        std::fs::write(tgt.path().join("README.md"), "stray").unwrap();

        let mut report = SyncReport::default();
        prune_target(tgt.path(), src.path(), false, &mut report).unwrap();
        // README.md is not a directory; loop continues. No pruning to do
        // either since u1 still exists in source.
        assert!(report.actions.is_empty());
        assert!(tgt.path().join("README.md").exists());
    }

    #[test]
    fn dry_run_does_not_prune() {
        // --prune --dry-run reports a Pruned action but leaves the file alone.
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();

        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let report = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: None,
            since: None,
            prune: true,
            dry_run: true,
            now: chrono::Utc::now(),
        })
        .unwrap();
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Pruned { .. })));
        assert!(
            tgt.path().join("slug").join("u1.jsonl").exists(),
            "dry-run prune must not delete"
        );
    }

    #[test]
    fn source_root_non_directory_entries_are_skipped() {
        // A stray file at `<source>/stray.txt` (alongside slug directories)
        // must not trip the enumerator.
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        std::fs::write(src.path().join("stray.txt"), "noise").unwrap();

        let report = run_default(tgt.path(), src.path()).unwrap();
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert_eq!(count_created(&report), 1);
    }

    #[cfg(unix)]
    #[test]
    fn update_path_records_error_when_target_dir_becomes_unwritable() {
        // First run succeeds, populating the target. Then the source is
        // modified (forcing Update on the next pass) and the target slug
        // directory is made read-only — copy_session fails inside the Update
        // arm, exercising the error-push branch.
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "first\n");
        run_default(tgt.path(), src.path()).unwrap();

        sb.append("slug", "u1", "second\n");

        let slug_dir = tgt.path().join("slug");
        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let report = run_default(tgt.path(), src.path()).unwrap();
        assert_eq!(
            report.errors.len(),
            1,
            "actions: {:?}, errors: {:?}",
            report.actions,
            report.errors
        );

        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn project_flag_accepts_leading_hyphen_via_clap() {
        // Real-world slug names (e.g. `-Users-jky-tmp`) start with `-`. Without
        // `allow_hyphen_values`, clap rejects them as unknown flags before the
        // command runs. This test pins that behaviour from the parsing side.
        let src = tempdir();
        let tgt = tempdir();
        let cmd = SyncCommand::try_parse_from([
            "history-sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            tgt.path().to_str().unwrap(),
            "--project",
            "-Users-jky-tmp",
        ])
        .unwrap();
        assert_eq!(cmd.project.as_deref(), Some("-Users-jky-tmp"));
    }

    #[test]
    fn since_with_invalid_value_errors() {
        let src = tempdir();
        let tgt = tempdir();
        let err = run(SyncOptions {
            target: tgt.path(),
            source: Some(src.path()),
            project: None,
            since: Some("nonsense"),
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("--since"));
    }

    #[cfg(unix)]
    #[test]
    fn plan_session_propagates_stat_permission_error() {
        // Force fs::metadata on the existing target file to fail with
        // PermissionDenied (not NotFound), exercising plan_session's
        // `Err(e) => Err(_).with_context(_)` arm and the upstream
        // `Err(e) => Err(e)` join in run().
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        run_default(tgt.path(), src.path()).unwrap();

        // 0o000 on the slug dir means fs::metadata on its children fails
        // with EACCES rather than ENOENT.
        let slug_dir = tgt.path().join("slug");
        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let report = run_default(tgt.path(), src.path());

        // Restore before assertions so TempDir cleans up regardless.
        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let report = report.unwrap();
        assert_eq!(
            report.errors.len(),
            1,
            "expected exactly one stat error, got actions={:?} errors={:?}",
            report.actions,
            report.errors
        );
        assert!(
            report.errors[0].reason.contains("Failed to stat"),
            "unexpected error: {}",
            report.errors[0].reason
        );
    }

    #[cfg(unix)]
    #[test]
    fn prune_propagates_read_dir_permission_error() {
        // Force prune_target's read_dir to fail with a non-NotFound error
        // (permission denied). Covers the `Err(e) => return Err(...)` arm of
        // the read_dir match in prune_target.
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        run_default(tgt.path(), src.path()).unwrap();

        // Mode 0o000: no read, no traverse — read_dir on this target fails
        // with PermissionDenied, not NotFound.
        std::fs::set_permissions(tgt.path(), std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut report = SyncReport::default();
        let result = prune_target(tgt.path(), src.path(), false, &mut report);

        // Restore perms before any assertion so TempDir can clean up even if we panic.
        std::fs::set_permissions(tgt.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Failed to read target"), "unexpected: {msg}");
    }
}
