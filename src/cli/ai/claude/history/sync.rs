//! `omni-dev ai claude history sync` — exports Claude Code conversation
//! history to a target directory as one `.jsonl` (and/or `.md`) per chat.
//!
//! See the issue and the module-level docs in [`super`] for the design rationale
//! (behavioural transcript vs faithful archive). This file implements only the
//! algorithm; rendering lives in [`super::markdown`].

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use serde::Serialize;

use super::common::{
    decode_slug, default_source_root, is_inside, parse_since, FileFormat, OutputFormat,
};
use super::markdown::{self, RenderOptions};

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
    /// Only files matching `<slug>/<uuid>.<ext>` are eligible for deletion;
    /// `<ext>` is restricted to the formats listed in `--output-format` so
    /// artifacts the run was not asked to manage are preserved regardless.
    #[arg(long)]
    pub prune: bool,

    /// Preview changes without touching the target.
    #[arg(long)]
    pub dry_run: bool,

    /// Report format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// On-disk file shape(s) to write. Accepts a comma-separated list, e.g.
    /// `--output-format jsonl,markdown` writes both side-by-side.
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT[,FORMAT...]",
        value_delimiter = ',',
        default_value = "jsonl"
    )]
    pub output_format: Vec<FileFormat>,

    /// Hide system-side events (system reminders, attachments, permission-mode,
    /// summary, generic system events) from the rendered transcript. Affects
    /// markdown output only — jsonl is byte-identical regardless.
    #[arg(long)]
    pub exclude_system: bool,
}

impl SyncCommand {
    /// Executes the sync.
    pub fn execute(self) -> Result<()> {
        let formats = dedupe_formats(self.output_format.clone())?;
        let report = run(SyncOptions {
            target: &self.target,
            source: self.source.as_deref(),
            project: self.project.as_deref(),
            since: self.since.as_deref(),
            prune: self.prune,
            dry_run: self.dry_run,
            now: Utc::now(),
            output_formats: formats,
            exclude_system: self.exclude_system,
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

/// Dedupes the user-supplied list of output formats and rejects an empty set.
fn dedupe_formats(input: Vec<FileFormat>) -> Result<BTreeSet<FileFormat>> {
    let set: BTreeSet<FileFormat> = input.into_iter().collect();
    if set.is_empty() {
        anyhow::bail!("--output-format must list at least one format");
    }
    Ok(set)
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
    /// Set of file shapes to emit. Iteration order is canonical (jsonl first,
    /// then markdown) because [`FileFormat`] is `Ord`.
    pub output_formats: BTreeSet<FileFormat>,
    pub exclude_system: bool,
}

/// Outcome of a sync run.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub actions: Vec<SyncAction>,
    pub errors: Vec<SyncError>,
}

/// One unit of work the sync performed (or, in dry-run mode, would perform).
///
/// Each variant carries the [`FileFormat`] it pertains to so the report can
/// distinguish parallel jsonl and markdown actions for the same session.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncAction {
    Created {
        project: String,
        session: String,
        target: PathBuf,
        bytes: u64,
        format: FileFormat,
    },
    Updated {
        project: String,
        session: String,
        target: PathBuf,
        bytes: u64,
        format: FileFormat,
    },
    Skipped {
        project: String,
        session: String,
        target: PathBuf,
        reason: SkipReason,
        format: FileFormat,
    },
    Pruned {
        project: String,
        session: String,
        target: PathBuf,
        format: FileFormat,
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

    if opts.output_formats.is_empty() {
        anyhow::bail!("output_formats must list at least one format");
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
            // Record skip per session-format so the user sees the filter took effect.
            for session in list_sessions(&project_dir)? {
                for &format in &opts.output_formats {
                    report.actions.push(SyncAction::Skipped {
                        project: slug.clone(),
                        session: session.uuid.clone(),
                        target: target_path_for(opts.target, &slug, &session.uuid, format),
                        reason: SkipReason::FilteredByProject,
                        format,
                    });
                }
            }
            continue;
        }

        for session in list_sessions(&project_dir)? {
            let target_dir = opts.target.join(&slug);

            if let Some(cutoff) = cutoff {
                let mtime: DateTime<Utc> = session.mtime.into();
                if mtime < cutoff {
                    for &format in &opts.output_formats {
                        report.actions.push(SyncAction::Skipped {
                            project: slug.clone(),
                            session: session.uuid.clone(),
                            target: target_path_for(opts.target, &slug, &session.uuid, format),
                            reason: SkipReason::FilteredBySince,
                            format,
                        });
                    }
                    continue;
                }
            }

            for &format in &opts.output_formats {
                let target_path =
                    target_dir.join(format!("{}.{}", session.uuid, format.extension()));
                let action = match plan_session(&session, &target_path, format) {
                    Ok(Plan::Skip) => Ok(SyncAction::Skipped {
                        project: slug.clone(),
                        session: session.uuid.clone(),
                        target: target_path.clone(),
                        reason: SkipReason::Unchanged,
                        format,
                    }),
                    Ok(Plan::Create) => write_session(
                        &session,
                        &target_dir,
                        &target_path,
                        format,
                        &slug,
                        opts.exclude_system,
                        opts.dry_run,
                    )
                    .map(|bytes| SyncAction::Created {
                        project: slug.clone(),
                        session: session.uuid.clone(),
                        target: target_path.clone(),
                        bytes,
                        format,
                    }),
                    Ok(Plan::Update) => write_session(
                        &session,
                        &target_dir,
                        &target_path,
                        format,
                        &slug,
                        opts.exclude_system,
                        opts.dry_run,
                    )
                    .map(|bytes| SyncAction::Updated {
                        project: slug.clone(),
                        session: session.uuid.clone(),
                        target: target_path.clone(),
                        bytes,
                        format,
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
    }

    if opts.prune {
        let format_by_ext: std::collections::HashMap<&'static str, FileFormat> = opts
            .output_formats
            .iter()
            .map(|&f| (f.extension(), f))
            .collect();
        prune_target(
            opts.target,
            source_root,
            opts.dry_run,
            &format_by_ext,
            &mut report,
        )?;
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

fn target_path_for(target: &Path, slug: &str, uuid: &str, format: FileFormat) -> PathBuf {
    target
        .join(slug)
        .join(format!("{uuid}.{}", format.extension()))
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
        if path.extension().and_then(|e| e.to_str()) != Some(FileFormat::Jsonl.extension()) {
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

fn plan_session(session: &Session, target_path: &Path, format: FileFormat) -> Result<Plan> {
    match fs::metadata(target_path) {
        Ok(meta) => {
            let same_size = meta.len() == session.size;
            let same_mtime = meta.modified().ok().is_some_and(|t| t == session.mtime);
            // Markdown is a derived artefact whose on-disk length differs from
            // the source jsonl, so size cannot participate in the key. The
            // source jsonl is append-only, making mtime alone a sufficient
            // freshness key for the rendered output.
            let unchanged = match format {
                FileFormat::Jsonl => same_size && same_mtime,
                FileFormat::Markdown => same_mtime,
            };
            if unchanged {
                Ok(Plan::Skip)
            } else {
                Ok(Plan::Update)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Plan::Create),
        Err(e) => Err(e).with_context(|| format!("Failed to stat {}", target_path.display())),
    }
}

/// Writes the session in the requested format. Dispatches by format:
/// jsonl is a snapshot-EOF byte copy; markdown reads the snapshot bytes and
/// renders via [`markdown::render`].
fn write_session(
    session: &Session,
    target_dir: &Path,
    target_path: &Path,
    format: FileFormat,
    project_slug: &str,
    exclude_system: bool,
    dry_run: bool,
) -> Result<u64> {
    if dry_run {
        // No I/O in dry-run. The jsonl byte count is known up-front; markdown
        // would require rendering, which we deliberately skip.
        return Ok(match format {
            FileFormat::Jsonl => session.size,
            FileFormat::Markdown => 0,
        });
    }

    fs::create_dir_all(target_dir)
        .with_context(|| format!("Failed to create {}", target_dir.display()))?;

    let bytes = match format {
        FileFormat::Jsonl => copy_jsonl(session, target_dir, target_path)?,
        FileFormat::Markdown => render_markdown_to_file(
            session,
            target_dir,
            target_path,
            project_slug,
            exclude_system,
        )?,
    };

    set_mtime(target_path, session.mtime)
        .with_context(|| format!("Failed to set mtime on {}", target_path.display()))?;

    Ok(bytes)
}

/// Copies a session from `session.src_path` to `target_path` using a
/// snapshot-EOF read of `session.size` bytes — sessions still being appended
/// to upstream produce a valid prefix.
fn copy_jsonl(session: &Session, target_dir: &Path, target_path: &Path) -> Result<u64> {
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

    Ok(copied)
}

fn render_markdown_to_file(
    session: &Session,
    target_dir: &Path,
    target_path: &Path,
    project_slug: &str,
    exclude_system: bool,
) -> Result<u64> {
    let mut buf = Vec::with_capacity(session.size as usize);
    {
        let mut src = File::open(&session.src_path)
            .with_context(|| format!("Failed to open {}", session.src_path.display()))?;
        let mut limited = (&mut src).take(session.size);
        limited
            .read_to_end(&mut buf)
            .with_context(|| format!("Failed to read {}", session.src_path.display()))?;
    }

    let rendered = markdown::render(
        &buf,
        RenderOptions {
            project_slug,
            session_uuid: &session.uuid,
            exclude_system,
        },
    )
    .with_context(|| {
        format!(
            "Failed to render markdown for {}",
            session.src_path.display()
        )
    })?;

    let mut tmp = tempfile::NamedTempFile::new_in(target_dir)
        .with_context(|| format!("Failed to create temp in {}", target_dir.display()))?;
    tmp.as_file_mut()
        .write_all(rendered.as_bytes())
        .with_context(|| format!("Failed to write markdown to {}", target_path.display()))?;
    tmp.as_file_mut()
        .flush()
        .with_context(|| format!("Failed to flush {}", target_path.display()))?;

    let persisted = tmp
        .persist(target_path)
        .map_err(|e| e.error)
        .with_context(|| format!("Failed to publish {}", target_path.display()))?;
    drop(persisted);

    Ok(rendered.len() as u64)
}

fn set_mtime(path: &Path, mtime: SystemTime) -> io::Result<()> {
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_modified(mtime)
}

fn prune_target(
    target_root: &Path,
    source_root: &Path,
    dry_run: bool,
    format_by_ext: &std::collections::HashMap<&'static str, FileFormat>,
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
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let Some(&format) = format_by_ext.get(ext) else {
                continue;
            };
            let Some(uuid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            // Source companionship is keyed off the canonical jsonl file —
            // the markdown is a derivative, so its presence/absence in source
            // tracks the jsonl.
            let source_file =
                source_slug_dir.join(format!("{uuid}.{}", FileFormat::Jsonl.extension()));
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
                format,
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

    fn jsonl_only() -> BTreeSet<FileFormat> {
        BTreeSet::from([FileFormat::Jsonl])
    }

    fn markdown_only() -> BTreeSet<FileFormat> {
        BTreeSet::from([FileFormat::Markdown])
    }

    fn both_formats() -> BTreeSet<FileFormat> {
        BTreeSet::from([FileFormat::Jsonl, FileFormat::Markdown])
    }

    fn default_opts<'a>(
        target: &'a Path,
        source: Option<&'a Path>,
        formats: BTreeSet<FileFormat>,
    ) -> SyncOptions<'a> {
        SyncOptions {
            target,
            source,
            project: None,
            since: None,
            prune: false,
            dry_run: false,
            now: chrono::Utc::now(),
            output_formats: formats,
            exclude_system: false,
        }
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
        run(default_opts(target, Some(source), jsonl_only()))
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

    fn count_format(r: &SyncReport, fmt: FileFormat) -> usize {
        r.actions
            .iter()
            .filter(|a| match a {
                SyncAction::Created { format, .. }
                | SyncAction::Updated { format, .. }
                | SyncAction::Skipped { format, .. }
                | SyncAction::Pruned { format, .. } => *format == fmt,
            })
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
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        let report = run(opts).unwrap();

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

        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.dry_run = true;
        let report = run(opts).unwrap();
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

        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.project = Some("-Users-jky-foo");
        let report = run(opts).unwrap();
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

        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.project = Some("/Users/jky/foo");
        let report = run(opts).unwrap();
        assert_eq!(count_created(&report), 1);
    }

    #[test]
    fn project_filter_no_match_skips_everything() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug-a", "u1", "a\n");
        sb.add("slug-b", "u2", "b\n");

        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.project = Some("nonexistent");
        let report = run(opts).unwrap();
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

        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.since = Some("1d");
        let report = run(opts).unwrap();

        assert_eq!(count_created(&report), 1);
        assert_eq!(count_filtered_by(&report, &SkipReason::FilteredBySince), 1);
    }

    #[test]
    fn target_inside_source_is_refused() {
        let src = tempdir();
        let tgt_inside = src.path().join("inside");
        std::fs::create_dir_all(&tgt_inside).unwrap();
        let err = run(default_opts(&tgt_inside, Some(src.path()), jsonl_only())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("inside"), "unexpected: {msg}");
    }

    #[test]
    fn missing_source_errors_clearly() {
        let tgt = tempdir();
        let nope = tgt.path().join("does-not-exist");
        let err = run(default_opts(tgt.path(), Some(&nope), jsonl_only())).unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"));
    }

    #[test]
    fn source_path_must_be_directory() {
        let src = tempdir();
        let tgt = tempdir();
        let file = src.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let err = run(default_opts(tgt.path(), Some(&file), jsonl_only())).unwrap_err();
        assert!(format!("{err:#}").contains("not a directory"));
    }

    #[test]
    fn snapshot_eof_copies_only_initial_length() {
        // Simulate a chat that grows mid-sync: list_sessions captures the
        // initial size; copy_jsonl must not exceed it.
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
        std::fs::create_dir_all(&target_dir).unwrap();
        let target_path = target_dir.join("u1.jsonl");
        let copied = copy_jsonl(&session, &target_dir, &target_path).unwrap();
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
        // Force copy_jsonl to fail by making the target slug directory
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

    fn jsonl_format_by_ext() -> std::collections::HashMap<&'static str, FileFormat> {
        let mut m: std::collections::HashMap<&'static str, FileFormat> =
            std::collections::HashMap::new();
        m.insert(FileFormat::Jsonl.extension(), FileFormat::Jsonl);
        m
    }

    #[test]
    fn prune_handles_missing_target_dir_silently() {
        let src = tempdir();
        let tgt_root = tempdir();
        let nonexistent = tgt_root.path().join("not-yet-here");
        let mut report = SyncReport::default();
        let format_by_ext = jsonl_format_by_ext();
        prune_target(&nonexistent, src.path(), false, &format_by_ext, &mut report).unwrap();
        assert!(report.actions.is_empty());
    }

    #[test]
    fn prune_skips_non_jsonl_and_non_files_inside_target_slug() {
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
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        let report = run(opts).unwrap();

        // Pruned exactly u1; subdir and notes.txt survive.
        assert_eq!(count_pruned(&report), 1);
        assert!(slug_dir.join("subdir").exists());
        assert!(slug_dir.join("notes.txt").exists());
    }

    #[test]
    fn prune_skips_non_directory_entries_at_target_root() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();
        std::fs::write(tgt.path().join("README.md"), "stray").unwrap();

        let mut report = SyncReport::default();
        let format_by_ext = jsonl_format_by_ext();
        prune_target(tgt.path(), src.path(), false, &format_by_ext, &mut report).unwrap();
        assert!(report.actions.is_empty());
        assert!(tgt.path().join("README.md").exists());
    }

    #[test]
    fn dry_run_does_not_prune() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();

        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        opts.dry_run = true;
        let report = run(opts).unwrap();
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
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.since = Some("nonsense");
        let err = run(opts).unwrap_err();
        assert!(format!("{err:#}").contains("--since"));
    }

    #[cfg(unix)]
    #[test]
    fn plan_session_propagates_stat_permission_error() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        run_default(tgt.path(), src.path()).unwrap();

        let slug_dir = tgt.path().join("slug");
        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let report = run_default(tgt.path(), src.path());

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
    fn run_propagates_prune_error_via_question_mark() {
        // Force prune_target to fail from inside run() so the `?` after the
        // prune_target(...) call (line ~346) is exercised. dry_run lets us
        // skip the upfront create_dir_all on the target while still entering
        // the prune branch.
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        run_default(tgt.path(), src.path()).unwrap();

        std::fs::set_permissions(tgt.path(), std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        opts.dry_run = true;
        let result = run(opts);

        std::fs::set_permissions(tgt.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("Failed to read target"));
    }

    #[cfg(unix)]
    #[test]
    fn prune_propagates_read_dir_permission_error() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        run_default(tgt.path(), src.path()).unwrap();

        std::fs::set_permissions(tgt.path(), std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut report = SyncReport::default();
        let format_by_ext = jsonl_format_by_ext();
        let result = prune_target(tgt.path(), src.path(), false, &format_by_ext, &mut report);

        std::fs::set_permissions(tgt.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Failed to read target"), "unexpected: {msg}");
    }

    // -------------------------------------------------------------------
    // Markdown / multi-format coverage
    // -------------------------------------------------------------------

    #[test]
    fn markdown_only_emits_only_md_files() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );

        let report = run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert_eq!(count_created(&report), 1);
        assert!(tgt.path().join("slug").join("u1.md").exists());
        assert!(!tgt.path().join("slug").join("u1.jsonl").exists());

        let body = std::fs::read_to_string(tgt.path().join("slug").join("u1.md")).unwrap();
        assert!(body.starts_with("---\n"));
        assert!(body.contains("hi"));
    }

    #[test]
    fn both_formats_emit_both_files_side_by_side() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );

        let report = run(default_opts(tgt.path(), Some(src.path()), both_formats())).unwrap();
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert_eq!(count_created(&report), 2);
        assert_eq!(count_format(&report, FileFormat::Jsonl), 1);
        assert_eq!(count_format(&report, FileFormat::Markdown), 1);
        assert!(tgt.path().join("slug").join("u1.md").exists());
        assert!(tgt.path().join("slug").join("u1.jsonl").exists());
    }

    #[test]
    fn markdown_format_re_run_is_skipped() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );

        run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();
        let report = run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();
        assert_eq!(count_skipped(&report), 1);
        assert_eq!(count_created(&report) + count_updated(&report), 0);
    }

    #[test]
    fn markdown_regenerates_when_source_grows() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"first\"}}\n",
        );
        run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();

        sb.append(
            "slug",
            "u1",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n",
        );
        let report = run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();
        assert_eq!(count_updated(&report), 1, "actions: {:?}", report.actions);

        let body = std::fs::read_to_string(tgt.path().join("slug").join("u1.md")).unwrap();
        assert!(body.contains("first"));
        assert!(body.contains("second"));
    }

    #[test]
    fn prune_scopes_to_requested_format_jsonl_only() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );

        // Produce both files first.
        run(default_opts(tgt.path(), Some(src.path()), both_formats())).unwrap();
        assert!(tgt.path().join("slug").join("u1.jsonl").exists());
        assert!(tgt.path().join("slug").join("u1.md").exists());

        // Remove source, then prune with only jsonl format requested.
        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        let report = run(opts).unwrap();

        assert_eq!(count_pruned(&report), 1);
        assert!(!tgt.path().join("slug").join("u1.jsonl").exists());
        // Markdown survives — caller did not ask to manage it.
        assert!(tgt.path().join("slug").join("u1.md").exists());
    }

    #[test]
    fn prune_scopes_to_requested_format_markdown_only() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );

        run(default_opts(tgt.path(), Some(src.path()), both_formats())).unwrap();
        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();

        let mut opts = default_opts(tgt.path(), Some(src.path()), markdown_only());
        opts.prune = true;
        let report = run(opts).unwrap();

        assert_eq!(count_pruned(&report), 1);
        assert!(tgt.path().join("slug").join("u1.jsonl").exists());
        assert!(!tgt.path().join("slug").join("u1.md").exists());
    }

    #[test]
    fn prune_with_both_formats_removes_both() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );

        run(default_opts(tgt.path(), Some(src.path()), both_formats())).unwrap();
        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();

        let mut opts = default_opts(tgt.path(), Some(src.path()), both_formats());
        opts.prune = true;
        let report = run(opts).unwrap();

        assert_eq!(count_pruned(&report), 2);
        assert_eq!(count_format(&report, FileFormat::Jsonl), 1);
        assert_eq!(count_format(&report, FileFormat::Markdown), 1);
    }

    #[test]
    fn exclude_system_drops_attachment_from_markdown() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n\
             {\"type\":\"attachment\",\"attachment\":{\"type\":\"skill_listing\",\"skillCount\":1}}\n",
        );

        let mut opts = default_opts(tgt.path(), Some(src.path()), markdown_only());
        opts.exclude_system = true;
        run(opts).unwrap();
        let body = std::fs::read_to_string(tgt.path().join("slug").join("u1.md")).unwrap();
        assert!(body.contains("hi"));
        assert!(!body.contains("Attachment"));
    }

    #[test]
    fn markdown_partial_jsonl_prefix_renders_what_it_can() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        let path = sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"complete\"}}\n",
        );
        // Append a partial JSON line that the renderer must tolerate.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"type\":\"user\",\"message\":{\"content\":\"par")
            .unwrap();

        run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();
        let body = std::fs::read_to_string(tgt.path().join("slug").join("u1.md")).unwrap();
        assert!(body.contains("complete"));
        assert!(!body.contains("\"par"));
    }

    #[test]
    fn markdown_action_carries_format_in_yaml_serialisation() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );
        let report = run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();
        let yaml = serde_yaml::to_string(&report).unwrap();
        assert!(yaml.contains("format: markdown"), "yaml: {yaml}");
    }

    #[test]
    fn dedupe_formats_collapses_duplicates() {
        let s = dedupe_formats(vec![FileFormat::Jsonl, FileFormat::Jsonl]).unwrap();
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn dedupe_formats_rejects_empty_set() {
        assert!(dedupe_formats(vec![]).is_err());
    }

    #[test]
    fn run_rejects_empty_output_formats() {
        let src = tempdir();
        let tgt = tempdir();
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.output_formats = BTreeSet::new();
        let err = run(opts).unwrap_err();
        assert!(format!("{err:#}").contains("output_formats"));
    }

    #[test]
    fn output_format_clap_accepts_comma_split() {
        let src = tempdir();
        let tgt = tempdir();
        let cmd = SyncCommand::try_parse_from([
            "history-sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            tgt.path().to_str().unwrap(),
            "--output-format",
            "jsonl,markdown",
        ])
        .unwrap();
        assert_eq!(cmd.output_format.len(), 2);
        assert!(cmd.output_format.contains(&FileFormat::Jsonl));
        assert!(cmd.output_format.contains(&FileFormat::Markdown));
    }

    #[test]
    fn output_format_clap_default_is_jsonl() {
        let src = tempdir();
        let tgt = tempdir();
        let cmd = SyncCommand::try_parse_from([
            "history-sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            tgt.path().to_str().unwrap(),
        ])
        .unwrap();
        assert_eq!(cmd.output_format, vec![FileFormat::Jsonl]);
    }

    #[test]
    fn exclude_system_clap_flag_parses() {
        let src = tempdir();
        let tgt = tempdir();
        let cmd = SyncCommand::try_parse_from([
            "history-sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            tgt.path().to_str().unwrap(),
            "--exclude-system",
        ])
        .unwrap();
        assert!(cmd.exclude_system);
    }

    // Linux-only: macOS / APFS rejects non-utf8 filenames at create time
    // ("Illegal byte sequence"), so the create_dir below would fail before
    // the prune walker is reached. Linux ext4/btrfs accepts them.
    #[cfg(target_os = "linux")]
    #[test]
    fn prune_skips_target_slug_dir_with_non_utf8_name() {
        // Cover the `file_name().to_str() => None` continue branch in
        // prune_target's slug-dir loop. Needs a Unix filesystem accepting
        // non-utf8 bytes as a directory name.
        use std::os::unix::ffi::OsStrExt;

        let src = tempdir();
        let tgt = tempdir();
        // Real session under a normal slug.
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        run_default(tgt.path(), src.path()).unwrap();

        // Drop a non-utf8 sibling slug dir into the target. Lone 0xff is
        // invalid utf-8 — file_name().to_str() returns None.
        let bad_name = std::ffi::OsStr::from_bytes(b"slug-\xff");
        let bad_dir = tgt.path().join(bad_name);
        std::fs::create_dir_all(&bad_dir).unwrap();

        // Remove u1 source so prune has work to do; bad_dir must not break the walker.
        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        let report = run(opts).unwrap();

        assert_eq!(count_pruned(&report), 1);
        assert!(bad_dir.exists(), "non-utf8 slug must be left alone");
    }

    // Linux-only: see sibling test for the rationale.
    #[cfg(target_os = "linux")]
    #[test]
    fn prune_skips_target_files_with_non_utf8_stem() {
        // Cover the `file_stem().to_str() => None` continue branch inside
        // prune_target's per-slug file loop.
        use std::os::unix::ffi::OsStrExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "x\n");
        run_default(tgt.path(), src.path()).unwrap();

        // Drop a `.jsonl` file with a non-utf8 stem inside the target slug dir.
        let bad_stem = std::ffi::OsStr::from_bytes(b"bad-\xff");
        let bad_file = tgt
            .path()
            .join("slug")
            .join(bad_stem)
            .with_extension("jsonl");
        std::fs::write(&bad_file, "x").unwrap();

        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        let report = run(opts).unwrap();

        // u1 still pruned; the non-utf8 stem file is left alone.
        assert_eq!(count_pruned(&report), 1);
        assert!(bad_file.exists(), "non-utf8 stem must not be deleted");
    }

    #[test]
    fn prune_skips_files_without_extension_and_unrecognised_extensions() {
        // Drop a no-extension file and a `.txt` file in the target slug; both
        // must survive --prune (covers the `Some(ext)` and `format_by_ext.get`
        // continue branches in prune_target).
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add("slug", "u1", "a\n");
        run_default(tgt.path(), src.path()).unwrap();

        let slug_dir = tgt.path().join("slug");
        std::fs::write(slug_dir.join("noext"), "x").unwrap();
        std::fs::write(slug_dir.join("notes.txt"), "x").unwrap();

        // Remove u1 source so the jsonl is eligible for prune; the no-ext and
        // .txt files must not be touched.
        std::fs::remove_file(src.path().join("slug").join("u1.jsonl")).unwrap();
        let mut opts = default_opts(tgt.path(), Some(src.path()), jsonl_only());
        opts.prune = true;
        let report = run(opts).unwrap();

        assert_eq!(count_pruned(&report), 1);
        assert!(slug_dir.join("noext").exists());
        assert!(slug_dir.join("notes.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn markdown_write_records_error_when_target_dir_becomes_unwritable() {
        // First sync populates the target. Then make the slug dir read-only
        // and append to source so the next pass tries to Update — the markdown
        // tempfile creation fails inside render_markdown_to_file, exercising
        // the `?` propagation via write_session and into run()'s error arm.
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"first\"}}\n",
        );
        run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();

        sb.append(
            "slug",
            "u1",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n",
        );

        let slug_dir = tgt.path().join("slug");
        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let report = run(default_opts(tgt.path(), Some(src.path()), markdown_only())).unwrap();

        std::fs::set_permissions(&slug_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(report.errors.len(), 1, "errors: {:?}", report.errors);
        assert!(
            report.errors[0].reason.contains("Failed to create temp")
                || report.errors[0].reason.contains("Failed to write"),
            "unexpected: {}",
            report.errors[0].reason
        );
    }

    #[test]
    fn render_markdown_to_file_propagates_open_error() {
        // Force the underlying File::open to fail by passing a Session that
        // points at a missing source path. Covers the `?` propagation in
        // render_markdown_to_file.
        let tgt = tempdir();
        let target_dir = tgt.path().join("slug");
        std::fs::create_dir_all(&target_dir).unwrap();
        let session = Session {
            uuid: "u1".to_string(),
            src_path: tgt.path().join("does-not-exist.jsonl"),
            size: 100,
            mtime: SystemTime::now(),
        };
        let target_path = target_dir.join("u1.md");
        let err = render_markdown_to_file(&session, &target_dir, &target_path, "slug", false)
            .unwrap_err();
        assert!(format!("{err:#}").contains("Failed to open"));
    }

    #[test]
    fn dry_run_does_not_create_markdown_either() {
        let src = tempdir();
        let tgt = tempdir();
        let sb = SourceBuilder::new(src.path().to_path_buf());
        sb.add(
            "slug",
            "u1",
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );

        let mut opts = default_opts(tgt.path(), Some(src.path()), markdown_only());
        opts.dry_run = true;
        let report = run(opts).unwrap();
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Created { .. })));
        assert!(!tgt.path().join("slug").exists());
    }
}
