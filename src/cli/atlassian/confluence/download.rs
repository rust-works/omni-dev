//! CLI command for recursively downloading Confluence page trees.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{ArgGroup, Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use crate::atlassian::api::AtlassianApi;
use crate::atlassian::confluence_api::{ChildPage, ConfluenceApi};
use crate::atlassian::document::content_item_to_document;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::create_client;

/// Behavior when a file already exists at the download target.
#[derive(Clone, Debug, Default, ValueEnum)]
pub enum OnConflict {
    /// Overwrite, but backup the old file first (default).
    #[default]
    Backup,
    /// Skip — don't touch existing files.
    Skip,
    /// Overwrite without backup.
    Overwrite,
}

/// Recursively downloads a Confluence page tree.
///
/// Takes either a single root page ID (positional) or a space key
/// (`--space`), from which every top-level page in the space becomes
/// a separate root in the download tree.
#[derive(Parser)]
#[command(group(
    ArgGroup::new("source")
        .required(true)
        .args(["id", "space"]),
))]
pub struct DownloadCommand {
    /// Root Confluence page ID to start from.
    pub id: Option<String>,

    /// Space key — downloads every top-level page in the space.
    #[arg(long)]
    pub space: Option<String>,

    /// Output directory.
    #[arg(long, alias = "output", default_value = ".")]
    pub output_dir: PathBuf,

    /// Output format per page.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Maximum concurrent downloads.
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,

    /// Maximum tree depth (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub max_depth: u32,

    /// Only download pages whose title contains this substring (case-insensitive).
    /// Children of non-matching pages are still traversed.
    #[arg(long)]
    pub title_filter: Option<String>,

    /// Use manifest for ID-aware resume (skip already-downloaded pages).
    #[arg(long)]
    pub resume: bool,

    /// What to do when a file already exists.
    #[arg(long, value_enum, default_value_t = OnConflict::Backup)]
    pub on_conflict: OnConflict,
}

/// A page to download.
struct PageTask {
    id: String,
    title: String,
    dir: PathBuf,
    depth: u32,
    parent_id: Option<String>,
}

/// Shared download configuration.
struct DownloadConfig {
    format: ContentFormat,
    ext: String,
    instance_url: String,
    on_conflict: OnConflict,
    backup_dir: PathBuf,
}

/// Download statistics.
struct DownloadStats {
    downloaded: AtomicUsize,
    skipped: AtomicUsize,
    clobbered: AtomicUsize,
    failed: AtomicUsize,
}

impl DownloadStats {
    fn new() -> Self {
        Self {
            downloaded: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            clobbered: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        }
    }
}

/// Manifest entry for a downloaded page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub title: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Per-page metadata written alongside the content file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMeta {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// A single log entry recorded per page action.
#[derive(Debug, Clone)]
struct LogEntry {
    action: String,
    id: String,
    path: String,
    detail: String,
}

/// Parameters for a recursive download, decoupled from CLI parsing.
struct DownloadParams {
    id: Option<String>,
    space: Option<String>,
    output_dir: PathBuf,
    format: ContentFormat,
    concurrency: usize,
    max_depth: u32,
    title_filter: Option<String>,
    resume: bool,
    on_conflict: OnConflict,
    instance_url: String,
}

impl DownloadParams {
    /// Builds params from a CLI command and the resolved instance URL.
    fn from_command(cmd: DownloadCommand, instance_url: String) -> Self {
        Self {
            id: cmd.id,
            space: cmd.space,
            output_dir: cmd.output_dir,
            format: cmd.format,
            concurrency: cmd.concurrency,
            max_depth: cmd.max_depth,
            title_filter: cmd.title_filter,
            resume: cmd.resume,
            on_conflict: cmd.on_conflict,
            instance_url,
        }
    }
}

impl DownloadCommand {
    /// Executes the recursive download.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = Arc::new(ConfluenceApi::new(client));
        let params = DownloadParams::from_command(self, instance_url);
        run_download(&api, &params).await
    }
}

/// Recursively downloads a Confluence page tree.
async fn run_download(api: &Arc<ConfluenceApi>, params: &DownloadParams) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(params.concurrency));
    let stats = Arc::new(DownloadStats::new());
    let log_entries: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let manifest_entries: Arc<Mutex<BTreeMap<String, ManifestEntry>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    let timestamp = Utc::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let backup_dir = params.output_dir.join(".backups").join(&timestamp);

    let title_filter = params.title_filter.as_ref().map(|s| s.to_lowercase());

    let config = Arc::new(DownloadConfig {
        ext: file_extension(&params.format),
        format: params.format.clone(),
        instance_url: params.instance_url.clone(),
        on_conflict: params.on_conflict.clone(),
        backup_dir,
    });

    // Load existing manifest for resume
    let old_manifest = if params.resume {
        load_manifest(&params.output_dir)
    } else {
        BTreeMap::new()
    };

    // Seed the queue. Either one explicit root page or every
    // top-level page in a space.
    let mut queue: VecDeque<PageTask> = VecDeque::new();
    let roots = seed_roots(api, params.id.as_deref(), params.space.as_deref()).await?;
    if roots.is_empty() {
        eprintln!("No pages to download.");
        return Ok(());
    }
    for root in roots {
        let slug = slugify(&root.title);
        let dir = params.output_dir.join(format!("{}-{}", root.id, slug));
        queue.push_back(PageTask {
            id: root.id,
            title: root.title,
            dir,
            depth: 0,
            parent_id: root.parent_id,
        });
    }

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    while let Some(task) = queue.pop_front() {
        // Title filter: when present, non-matching pages are skipped
        // (not downloaded) but their children are still enumerated.
        if !title_matches(&title_filter, &task.title) {
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            eprintln!("  Skipped (filter): {} - {}", task.id, task.title);
            log_entries.lock().await.push(LogEntry {
                action: "skipped".to_string(),
                id: task.id.clone(),
                path: String::new(),
                detail: "title-filter".to_string(),
            });
            enqueue_children(api, &task, params.max_depth, &mut queue).await;
            continue;
        }

        let relative_path = task
            .dir
            .strip_prefix(&params.output_dir)
            .unwrap_or(&task.dir)
            .join(format!("index.{}", config.ext))
            .to_string_lossy()
            .to_string();

        // Resume: check manifest for ID-based skip
        if params.resume {
            if let Some(entry) = old_manifest.get(&task.id) {
                if entry.path == relative_path {
                    // Same path — skip
                    stats.skipped.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  Skipped (resume): {} - {}", task.id, task.title);
                    log_entries.lock().await.push(LogEntry {
                        action: "skipped".to_string(),
                        id: task.id.clone(),
                        path: relative_path.clone(),
                        detail: "resume".to_string(),
                    });
                    // Still record in new manifest
                    manifest_entries.lock().await.insert(
                        task.id.clone(),
                        ManifestEntry {
                            title: task.title.clone(),
                            path: relative_path,
                            parent_id: task.parent_id.clone(),
                        },
                    );
                    // Still need to discover children
                } else {
                    // Page moved — download to new path (old path becomes orphan)
                    eprintln!(
                        "  Moved: {} - {} (was: {})",
                        task.id, task.title, entry.path
                    );
                    log_entries.lock().await.push(LogEntry {
                        action: "moved".to_string(),
                        id: task.id.clone(),
                        path: relative_path.clone(),
                        detail: format!("was: {}", entry.path),
                    });
                    // Fall through to download
                    spawn_download(
                        &mut handles,
                        api,
                        &semaphore,
                        &stats,
                        &config,
                        &log_entries,
                        &manifest_entries,
                        &task,
                        &relative_path,
                    );
                }
            } else {
                // Not in manifest — new page, download
                spawn_download(
                    &mut handles,
                    api,
                    &semaphore,
                    &stats,
                    &config,
                    &log_entries,
                    &manifest_entries,
                    &task,
                    &relative_path,
                );
            }
        } else {
            spawn_download(
                &mut handles,
                api,
                &semaphore,
                &stats,
                &config,
                &log_entries,
                &manifest_entries,
                &task,
                &relative_path,
            );
        }

        enqueue_children(api, &task, params.max_depth, &mut queue).await;
    }

    // Await all download tasks
    for handle in handles {
        let _ = handle.await;
    }

    // Write manifest
    let manifest = manifest_entries.lock().await;
    write_manifest(&params.output_dir, &manifest);

    // Write log
    let entries = log_entries.lock().await;
    write_log(&params.output_dir, &timestamp, &entries, &stats);

    // Summary
    let downloaded = stats.downloaded.load(Ordering::Relaxed);
    let skipped = stats.skipped.load(Ordering::Relaxed);
    let clobbered = stats.clobbered.load(Ordering::Relaxed);
    let failed = stats.failed.load(Ordering::Relaxed);

    eprintln!(
            "\nDone. Downloaded: {downloaded}, Clobbered: {clobbered}, Skipped: {skipped}, Failed: {failed}"
        );

    if failed > 0 {
        anyhow::bail!("{failed} page(s) failed to download");
    }

    Ok(())
}

/// A candidate root page for seeding the download queue.
#[derive(Debug)]
struct RootPage {
    id: String,
    title: String,
    parent_id: Option<String>,
}

/// Resolves the starting set of pages. Either a single root page ID
/// is given, or a space key from which every top-level page is used.
async fn seed_roots(
    api: &ConfluenceApi,
    id: Option<&str>,
    space: Option<&str>,
) -> Result<Vec<RootPage>> {
    if let Some(space_key) = space {
        eprintln!("Resolving space {space_key}...");
        let space_id = api
            .resolve_space_id(space_key)
            .await
            .with_context(|| format!("Failed to resolve space key \"{space_key}\""))?;
        eprintln!("Listing root pages in space {space_key}...");
        let pages = api.get_space_root_pages(&space_id).await?;
        eprintln!("Found {} root page(s) in space {space_key}.", pages.len());
        return Ok(pages
            .into_iter()
            .map(|p| RootPage {
                id: p.id,
                title: p.title,
                parent_id: None,
            })
            .collect());
    }

    let id = id.context("either a page ID or --space must be provided")?;
    eprintln!("Fetching root page {id}...");
    let root = api.get_content(id).await?;
    Ok(vec![RootPage {
        parent_id: metadata_parent_id(&root.metadata),
        id: root.id,
        title: root.title,
    }])
}

/// Extracts the parent page ID from a content metadata variant.
///
/// Only the Confluence variant carries a parent page reference; any other
/// variant returns `None`.
fn metadata_parent_id(metadata: &crate::atlassian::api::ContentMetadata) -> Option<String> {
    match metadata {
        crate::atlassian::api::ContentMetadata::Confluence { parent_id, .. } => parent_id.clone(),
        crate::atlassian::api::ContentMetadata::Jira { .. } => None,
    }
}

/// Fetches a page's children and enqueues them for download.
///
/// Tolerates child-listing failures by logging a warning — one broken
/// sub-tree shouldn't abort a whole-space download.
async fn enqueue_children(
    api: &ConfluenceApi,
    task: &PageTask,
    max_depth: u32,
    queue: &mut VecDeque<PageTask>,
) {
    if max_depth > 0 && task.depth >= max_depth {
        return;
    }
    match api.get_children(&task.id).await {
        Ok(children) => {
            for child in children {
                queue.push_back(child_to_task(child, task));
            }
        }
        Err(e) => {
            eprintln!(
                "WARNING: Failed to fetch children of {} ({}): {}",
                task.id, task.title, e
            );
        }
    }
}

/// Converts a `ChildPage` into a `PageTask` under the parent's directory.
fn child_to_task(child: ChildPage, parent: &PageTask) -> PageTask {
    let slug = slugify(&child.title);
    let dir = parent.dir.join(format!("{}-{}", child.id, slug));
    PageTask {
        id: child.id,
        title: child.title,
        dir,
        depth: parent.depth + 1,
        parent_id: Some(parent.id.clone()),
    }
}

/// Returns true when the page title matches the optional title filter.
///
/// An absent filter matches everything; a present filter does a
/// case-insensitive substring match.
fn title_matches(filter: &Option<String>, title: &str) -> bool {
    match filter {
        None => true,
        Some(needle) => title.to_lowercase().contains(needle),
    }
}

/// Spawns a download task for a page.
#[allow(clippy::too_many_arguments)]
fn spawn_download(
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
    api: &Arc<ConfluenceApi>,
    semaphore: &Arc<Semaphore>,
    stats: &Arc<DownloadStats>,
    config: &Arc<DownloadConfig>,
    log_entries: &Arc<Mutex<Vec<LogEntry>>>,
    manifest_entries: &Arc<Mutex<BTreeMap<String, ManifestEntry>>>,
    task: &PageTask,
    relative_path: &str,
) {
    let api_clone = Arc::clone(api);
    let sem_clone = Arc::clone(semaphore);
    let stats_clone = Arc::clone(stats);
    let config_clone = Arc::clone(config);
    let log_clone = Arc::clone(log_entries);
    let manifest_clone = Arc::clone(manifest_entries);
    let task_id = task.id.clone();
    let task_title = task.title.clone();
    let task_dir = task.dir.clone();
    let task_parent = task.parent_id.clone();
    let rel_path = relative_path.to_string();

    handles.push(tokio::spawn(async move {
        let Ok(_permit) = sem_clone.acquire().await else {
            return;
        };
        let result = download_page(
            &api_clone,
            &task_id,
            &task_title,
            &task_dir,
            &task_parent,
            &config_clone,
            &stats_clone,
            &log_clone,
            &rel_path,
        )
        .await;

        if result {
            manifest_clone.lock().await.insert(
                task_id.clone(),
                ManifestEntry {
                    title: task_title,
                    path: rel_path,
                    parent_id: task_parent,
                },
            );
        }
    }));
}

/// Downloads a single page. Returns true if successfully recorded (downloaded or clobbered).
#[allow(clippy::too_many_arguments)]
async fn download_page(
    api: &ConfluenceApi,
    id: &str,
    title: &str,
    dir: &Path,
    parent_id: &Option<String>,
    config: &DownloadConfig,
    stats: &DownloadStats,
    log_entries: &Mutex<Vec<LogEntry>>,
    relative_path: &str,
) -> bool {
    let output_path = dir.join(format!("index.{}", config.ext));
    let meta_path = dir.join("meta.json");

    // Handle existing file
    if output_path.exists() {
        match config.on_conflict {
            OnConflict::Skip => {
                stats.skipped.fetch_add(1, Ordering::Relaxed);
                eprintln!("  Skipped (exists): {id} - {title}");
                log_entries.lock().await.push(LogEntry {
                    action: "skipped".to_string(),
                    id: id.to_string(),
                    path: relative_path.to_string(),
                    detail: "file exists".to_string(),
                });
                return true; // still counts as "present"
            }
            OnConflict::Backup => {
                if let Err(e) = backup_file(&output_path, &config.backup_dir, relative_path).await {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  FAILED backup: {id} - {title}: {e}");
                    log_entries.lock().await.push(LogEntry {
                        action: "failed".to_string(),
                        id: id.to_string(),
                        path: relative_path.to_string(),
                        detail: format!("backup failed: {e}"),
                    });
                    return false;
                }
                stats.clobbered.fetch_add(1, Ordering::Relaxed);
                let backup_path = config.backup_dir.join(relative_path);
                log_entries.lock().await.push(LogEntry {
                    action: "clobbered".to_string(),
                    id: id.to_string(),
                    path: relative_path.to_string(),
                    detail: format!("backup: {}", backup_path.display()),
                });
            }
            OnConflict::Overwrite => {
                // Overwrite without backup — no special action
            }
        }
    }

    // Fetch page content
    let item = match api.get_content(id).await {
        Ok(item) => item,
        Err(e) => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            eprintln!("  FAILED: {id} - {title}: {e}");
            log_entries.lock().await.push(LogEntry {
                action: "failed".to_string(),
                id: id.to_string(),
                path: relative_path.to_string(),
                detail: format!("{e}"),
            });
            return false;
        }
    };

    // Convert to output format
    let content = match config.format {
        ContentFormat::Adf => {
            match serde_json::to_string_pretty(&item.body_adf.unwrap_or(serde_json::Value::Null)) {
                Ok(json) => json,
                Err(e) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  FAILED: {id} - {title}: {e}");
                    return false;
                }
            }
        }
        ContentFormat::Jfm => {
            let doc = match content_item_to_document(&item, &config.instance_url) {
                Ok(doc) => doc,
                Err(e) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  FAILED: {id} - {title}: {e}");
                    return false;
                }
            };
            match doc.render() {
                Ok(rendered) => rendered,
                Err(e) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  FAILED: {id} - {title}: {e}");
                    return false;
                }
            }
        }
    };

    // Create directory
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        stats.failed.fetch_add(1, Ordering::Relaxed);
        eprintln!("  FAILED: {id} - {title}: {e}");
        return false;
    }

    // Write content file
    if let Err(e) = tokio::fs::write(&output_path, &content).await {
        stats.failed.fetch_add(1, Ordering::Relaxed);
        eprintln!("  FAILED: {id} - {title}: {e}");
        return false;
    }

    // Write meta.json
    let meta = PageMeta {
        id: id.to_string(),
        title: title.to_string(),
        parent_id: parent_id.clone(),
    };
    if let Ok(meta_json) = serde_json::to_string_pretty(&meta) {
        if let Err(e) = tokio::fs::write(&meta_path, meta_json).await {
            // meta.json failure = page failure (don't record in manifest)
            stats.failed.fetch_add(1, Ordering::Relaxed);
            eprintln!("  FAILED meta.json: {id} - {title}: {e}");
            return false;
        }
    }

    stats.downloaded.fetch_add(1, Ordering::Relaxed);
    eprintln!("  Downloaded: {id} - {title}");
    log_entries.lock().await.push(LogEntry {
        action: "downloaded".to_string(),
        id: id.to_string(),
        path: relative_path.to_string(),
        detail: String::new(),
    });

    true
}

/// Backs up a file before overwriting it.
async fn backup_file(source: &Path, backup_dir: &Path, relative_path: &str) -> Result<()> {
    let dest = backup_dir.join(relative_path);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("Failed to create backup directory")?;
    }
    tokio::fs::copy(source, &dest)
        .await
        .context("Failed to copy file to backup")?;
    Ok(())
}

/// Loads an existing manifest from the output directory.
fn load_manifest(output_dir: &Path) -> BTreeMap<String, ManifestEntry> {
    let path = output_dir.join("manifest.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Writes the manifest to the output directory.
fn write_manifest(output_dir: &Path, manifest: &BTreeMap<String, ManifestEntry>) {
    let path = output_dir.join("manifest.json");
    if let Ok(json) = serde_json::to_string_pretty(manifest) {
        let _ = std::fs::write(&path, json);
    }
}

/// Appends a run block to the download log.
fn write_log(output_dir: &Path, timestamp: &str, entries: &[LogEntry], stats: &DownloadStats) {
    use std::fmt::Write;
    let path = output_dir.join("download.log");

    let mut buf = String::new();
    let _ = writeln!(buf, "\n=== {timestamp} ===");
    for entry in entries {
        if entry.detail.is_empty() {
            let _ = writeln!(buf, "[{}]  {}  {}", entry.action, entry.id, entry.path);
        } else {
            let _ = writeln!(
                buf,
                "[{}]  {}  {}  ({})",
                entry.action, entry.id, entry.path, entry.detail
            );
        }
    }
    let downloaded = stats.downloaded.load(Ordering::Relaxed);
    let clobbered = stats.clobbered.load(Ordering::Relaxed);
    let skipped = stats.skipped.load(Ordering::Relaxed);
    let failed = stats.failed.load(Ordering::Relaxed);
    let _ = writeln!(
        buf,
        "=== done: {downloaded} downloaded, {clobbered} clobbered, {skipped} skipped, {failed} failed ==="
    );

    // Append to log file
    if let Ok(mut existing) = std::fs::read_to_string(&path) {
        existing.push_str(&buf);
        let _ = std::fs::write(&path, existing);
    } else {
        let _ = std::fs::write(&path, buf);
    }
}

/// Generates a filesystem-safe slug from a page title.
fn slugify(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    let mut result = String::with_capacity(slug.len());
    let mut prev_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    let trimmed = result.trim_matches('-');
    if trimmed.len() > 60 {
        trimmed[..60].trim_end_matches('-').to_string()
    } else {
        trimmed.to_string()
    }
}

/// Returns the file extension for the given format.
fn file_extension(format: &ContentFormat) -> String {
    match format {
        ContentFormat::Jfm => "md".to_string(),
        ContentFormat::Adf => "json".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── slugify ────────────────────────────────────────────────────

    #[test]
    fn slugify_normal_title() {
        assert_eq!(slugify("Team Handbook"), "team-handbook");
    }

    #[test]
    fn slugify_special_characters() {
        assert_eq!(
            slugify("Q1 2026 (Retro) — Summary!"),
            "q1-2026-retro-summary"
        );
    }

    #[test]
    fn slugify_consecutive_hyphens() {
        assert_eq!(slugify("a   b---c"), "a-b-c");
    }

    #[test]
    fn slugify_leading_trailing() {
        assert_eq!(slugify("  hello  "), "hello");
    }

    #[test]
    fn slugify_empty() {
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn slugify_long_title() {
        let long_title = "a".repeat(100);
        let slug = slugify(&long_title);
        assert!(slug.len() <= 60);
    }

    #[test]
    fn slugify_only_special_chars() {
        assert_eq!(slugify("!@#$%"), "");
    }

    #[test]
    fn slugify_unicode() {
        assert_eq!(slugify("日本語テスト"), "");
    }

    // ── file_extension ─────────────────────────────────────────────

    #[test]
    fn extension_jfm() {
        assert_eq!(file_extension(&ContentFormat::Jfm), "md");
    }

    #[test]
    fn extension_adf() {
        assert_eq!(file_extension(&ContentFormat::Adf), "json");
    }

    // ── OnConflict ─────────────────────────────────────────────────

    #[test]
    fn on_conflict_default_is_backup() {
        assert!(matches!(OnConflict::default(), OnConflict::Backup));
    }

    // ── ManifestEntry ──────────────────────────────────────────────

    #[test]
    fn manifest_entry_serialization() {
        let entry = ManifestEntry {
            title: "Test Page".to_string(),
            path: "12345-test-page/index.json".to_string(),
            parent_id: Some("99999".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("Test Page"));
        assert!(json.contains("parent_id"));
    }

    #[test]
    fn manifest_entry_without_parent() {
        let entry = ManifestEntry {
            title: "Root".to_string(),
            path: "root/index.json".to_string(),
            parent_id: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("parent_id"));
    }

    #[test]
    fn manifest_roundtrip() {
        let mut manifest = BTreeMap::new();
        manifest.insert(
            "12345".to_string(),
            ManifestEntry {
                title: "Page".to_string(),
                path: "12345-page/index.json".to_string(),
                parent_id: None,
            },
        );
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let restored: BTreeMap<String, ManifestEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored["12345"].title, "Page");
    }

    // ── PageMeta ───────────────────────────────────────────────────

    #[test]
    fn page_meta_serialization() {
        let meta = PageMeta {
            id: "12345".to_string(),
            title: "Full Title With Special Chars (test)".to_string(),
            parent_id: Some("99999".to_string()),
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        assert!(json.contains("Full Title With Special Chars (test)"));
    }

    // ── load_manifest ──────────────────────────────────────────────

    #[test]
    fn load_manifest_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = load_manifest(dir.path());
        assert!(manifest.is_empty());
    }

    #[test]
    fn load_manifest_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            r#"{"12345":{"title":"Page","path":"12345-page/index.json"}}"#,
        )
        .unwrap();
        let manifest = load_manifest(dir.path());
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest["12345"].title, "Page");
    }

    #[test]
    fn load_manifest_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, "not json").unwrap();
        let manifest = load_manifest(dir.path());
        assert!(manifest.is_empty());
    }

    // ── write_manifest ─────────────────────────────────────────────

    #[test]
    fn write_manifest_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = BTreeMap::new();
        manifest.insert(
            "12345".to_string(),
            ManifestEntry {
                title: "Test".to_string(),
                path: "12345-test/index.json".to_string(),
                parent_id: None,
            },
        );
        write_manifest(dir.path(), &manifest);
        let content = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
        assert!(content.contains("12345"));
    }

    // ── write_log ──────────────────────────────────────────────────

    #[test]
    fn write_log_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let stats = DownloadStats::new();
        stats.downloaded.store(2, Ordering::Relaxed);
        let entries = vec![LogEntry {
            action: "downloaded".to_string(),
            id: "12345".to_string(),
            path: "12345-page/index.json".to_string(),
            detail: String::new(),
        }];
        write_log(dir.path(), "2026-04-12T10-00-00", &entries, &stats);
        let content = std::fs::read_to_string(dir.path().join("download.log")).unwrap();
        assert!(content.contains("=== 2026-04-12T10-00-00 ==="));
        assert!(content.contains("[downloaded]"));
        assert!(content.contains("2 downloaded"));
    }

    #[test]
    fn write_log_appends() {
        let dir = tempfile::tempdir().unwrap();
        let stats = DownloadStats::new();
        let entries = vec![];
        write_log(dir.path(), "run1", &entries, &stats);
        write_log(dir.path(), "run2", &entries, &stats);
        let content = std::fs::read_to_string(dir.path().join("download.log")).unwrap();
        assert!(content.contains("run1"));
        assert!(content.contains("run2"));
    }

    // ── backup_file ────────────────────────────────────────────────

    #[tokio::test]
    async fn backup_file_copies_content() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("original.txt");
        std::fs::write(&source, "original content").unwrap();

        let backup_dir = dir.path().join(".backups/test");
        backup_file(&source, &backup_dir, "original.txt")
            .await
            .unwrap();

        let backed_up = std::fs::read_to_string(backup_dir.join("original.txt")).unwrap();
        assert_eq!(backed_up, "original content");
    }

    // ── DownloadCommand struct ─────────────────────────────────────

    #[test]
    fn download_command_defaults() {
        let cmd = DownloadCommand {
            id: Some("12345".to_string()),
            space: None,
            output_dir: PathBuf::from("."),
            format: ContentFormat::Jfm,
            concurrency: 8,
            max_depth: 0,
            title_filter: None,
            resume: false,
            on_conflict: OnConflict::Backup,
        };
        assert_eq!(cmd.id.as_deref(), Some("12345"));
        assert!(cmd.space.is_none());
        assert_eq!(cmd.concurrency, 8);
        assert!(!cmd.resume);
        assert!(cmd.title_filter.is_none());
        assert!(matches!(cmd.on_conflict, OnConflict::Backup));
    }

    // ── metadata_parent_id ─────────────────────────────────────────

    #[test]
    fn metadata_parent_id_confluence_some() {
        let meta = crate::atlassian::api::ContentMetadata::Confluence {
            space_key: "ENG".to_string(),
            status: Some("current".to_string()),
            version: Some(1),
            parent_id: Some("999".to_string()),
        };
        assert_eq!(metadata_parent_id(&meta).as_deref(), Some("999"));
    }

    #[test]
    fn metadata_parent_id_confluence_none() {
        let meta = crate::atlassian::api::ContentMetadata::Confluence {
            space_key: "ENG".to_string(),
            status: None,
            version: None,
            parent_id: None,
        };
        assert!(metadata_parent_id(&meta).is_none());
    }

    #[test]
    fn metadata_parent_id_jira() {
        let meta = crate::atlassian::api::ContentMetadata::Jira {
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: Vec::new(),
        };
        assert!(metadata_parent_id(&meta).is_none());
    }

    // ── DownloadParams::from_command ───────────────────────────────

    #[test]
    fn from_command_copies_all_fields() {
        let cmd = DownloadCommand {
            id: Some("42".to_string()),
            space: Some("ENG".to_string()),
            output_dir: PathBuf::from("/tmp/out"),
            format: ContentFormat::Adf,
            concurrency: 4,
            max_depth: 7,
            title_filter: Some("needle".to_string()),
            resume: true,
            on_conflict: OnConflict::Overwrite,
        };
        let params = DownloadParams::from_command(cmd, "https://org.atlassian.net".to_string());

        assert_eq!(params.id.as_deref(), Some("42"));
        assert_eq!(params.space.as_deref(), Some("ENG"));
        assert_eq!(params.output_dir, PathBuf::from("/tmp/out"));
        assert_eq!(params.concurrency, 4);
        assert_eq!(params.max_depth, 7);
        assert_eq!(params.title_filter.as_deref(), Some("needle"));
        assert!(params.resume);
        assert!(matches!(params.format, ContentFormat::Adf));
        assert!(matches!(params.on_conflict, OnConflict::Overwrite));
        assert_eq!(params.instance_url, "https://org.atlassian.net");
    }

    // ── DownloadCommand::execute ───────────────────────────────────
    //
    // `execute` is a thin shim over `create_client()` + `run_download`
    // (see STYLE-0025), but its body still participates in patch
    // coverage. To cover it we point credentials at a wiremock server
    // that returns 404, then assert the expected error bubbles up.
    //
    // Env vars are process-global, so serialise this test behind a
    // mutex — the same pattern used in `src/atlassian/auth.rs` tests.

    #[tokio::test(flavor = "current_thread")]
    async fn execute_runs_with_env_credentials() {
        use std::sync::Mutex;
        static ENV_MUTEX: Mutex<()> = Mutex::new(());
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        std::env::set_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL, server.uri());
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_EMAIL, "user@test.com");
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN, "t");

        let temp = tempfile::tempdir().unwrap();
        let cmd = DownloadCommand {
            id: Some("99999".to_string()),
            space: None,
            output_dir: temp.path().to_path_buf(),
            format: ContentFormat::Jfm,
            concurrency: 1,
            max_depth: 0,
            title_filter: None,
            resume: false,
            on_conflict: OnConflict::Overwrite,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("404"));

        // Clean up — leaves the process in a known state for any other
        // test that inspects these env vars (all guarded by ENV_MUTEX).
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_EMAIL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN);
    }

    // ── title_matches ──────────────────────────────────────────────

    #[test]
    fn title_matches_absent_filter_accepts_all() {
        assert!(title_matches(&None, "anything"));
        assert!(title_matches(&None, ""));
    }

    #[test]
    fn title_matches_exact_substring() {
        let filter = Some("architecture".to_string());
        assert!(title_matches(&filter, "System architecture overview"));
    }

    #[test]
    fn title_matches_case_insensitive() {
        let filter = Some("retro".to_string());
        assert!(title_matches(&filter, "Q1 2026 RETRO Summary"));
    }

    #[test]
    fn title_matches_no_match() {
        let filter = Some("auth".to_string());
        assert!(!title_matches(&filter, "Deployment Guide"));
    }

    #[test]
    fn title_matches_empty_filter_matches_all() {
        let filter = Some(String::new());
        assert!(title_matches(&filter, "anything"));
    }

    // ── child_to_task ──────────────────────────────────────────────

    #[test]
    fn child_to_task_builds_nested_dir() {
        let parent = PageTask {
            id: "100".to_string(),
            title: "Parent".to_string(),
            dir: PathBuf::from("out/100-parent"),
            depth: 1,
            parent_id: None,
        };
        let child = ChildPage {
            id: "200".to_string(),
            title: "Child Page".to_string(),
            status: String::new(),
            parent_id: Some("100".to_string()),
            space_key: None,
        };
        let task = child_to_task(child, &parent);

        assert_eq!(task.id, "200");
        assert_eq!(task.depth, 2);
        assert_eq!(task.parent_id.as_deref(), Some("100"));
        assert_eq!(task.dir, PathBuf::from("out/100-parent/200-child-page"));
    }

    // ── seed_roots ─────────────────────────────────────────────────

    #[tokio::test]
    async fn seed_roots_from_page_id() {
        let server = wiremock::MockServer::start().await;
        mock_leaf_page(&server, "12345").await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let roots = seed_roots(&api, Some("12345"), None).await.unwrap();

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, "12345");
    }

    #[tokio::test]
    async fn seed_roots_from_space_key() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "AD"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        // The v2 `depth=root` endpoint returns only root pages, so the
        // fixture here mirrors that — the API, not our code, does the
        // filtering.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "111", "title": "Overview"},
                        {"id": "333", "title": "Orphan Root"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let roots = seed_roots(&api, None, Some("AD")).await.unwrap();

        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].id, "111");
        assert_eq!(roots[1].id, "333");
    }

    #[tokio::test]
    async fn seed_roots_unknown_space_errors() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let err = seed_roots(&api, None, Some("NOPE")).await.unwrap_err();
        assert!(err.to_string().contains("NOPE"));
    }

    #[tokio::test]
    async fn seed_roots_neither_id_nor_space() {
        let server = wiremock::MockServer::start().await;
        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let err = seed_roots(&api, None, None).await.unwrap_err();
        assert!(err.to_string().contains("page ID or --space"));
    }

    // ── enqueue_children ───────────────────────────────────────────

    #[tokio::test]
    async fn enqueue_children_respects_max_depth() {
        let server = wiremock::MockServer::start().await;
        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let task = PageTask {
            id: "100".to_string(),
            title: "Leaf".to_string(),
            dir: PathBuf::from("out/100-leaf"),
            depth: 3,
            parent_id: None,
        };
        let mut queue: VecDeque<PageTask> = VecDeque::new();
        enqueue_children(&api, &task, 3, &mut queue).await;
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn enqueue_children_swallows_errors() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/100/child/page",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("oops"))
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let parent = PageTask {
            id: "100".to_string(),
            title: "Parent".to_string(),
            dir: PathBuf::from("out/100-parent"),
            depth: 0,
            parent_id: None,
        };
        let mut queue: VecDeque<PageTask> = VecDeque::new();
        enqueue_children(&api, &parent, 0, &mut queue).await;
        assert!(queue.is_empty());
    }

    // ── DownloadStats ──────────────────────────────────────────────

    #[test]
    fn download_stats_new() {
        let stats = DownloadStats::new();
        assert_eq!(stats.downloaded.load(Ordering::Relaxed), 0);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
        assert_eq!(stats.clobbered.load(Ordering::Relaxed), 0);
        assert_eq!(stats.failed.load(Ordering::Relaxed), 0);
    }

    // ── run_download ───────────────────────────────────────────────

    /// Mocks the page + space endpoints for a leaf page with no children.
    async fn mock_leaf_page(server: &wiremock::MockServer, id: &str) {
        let page_json = serde_json::json!({
            "id": id,
            "title": "Root Page",
            "status": "current",
            "spaceId": "98765",
            "version": {"number": 1},
            "body": {
                "atlas_doc_format": {
                    "value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"
                }
            }
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&page_json))
            .mount(server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/rest/api/content/{id}/child/page"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [], "_links": {}})),
            )
            .mount(server)
            .await;
    }

    /// Default parameters for tests — override the fields you care about.
    fn base_params(output_dir: &Path, instance_url: String) -> DownloadParams {
        DownloadParams {
            id: Some("12345".to_string()),
            space: None,
            output_dir: output_dir.to_path_buf(),
            format: ContentFormat::Jfm,
            concurrency: 2,
            max_depth: 0,
            title_filter: None,
            resume: false,
            on_conflict: OnConflict::Overwrite,
            instance_url,
        }
    }

    fn build_arc_api(server: &wiremock::MockServer) -> Arc<ConfluenceApi> {
        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        Arc::new(ConfluenceApi::new(client))
    }

    #[tokio::test]
    async fn run_download_leaf_page_jfm() {
        let server = wiremock::MockServer::start().await;
        mock_leaf_page(&server, "12345").await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        let params = base_params(temp.path(), server.uri());

        assert!(run_download(&api, &params).await.is_ok());
        assert!(temp.path().join("manifest.json").exists());
    }

    #[tokio::test]
    async fn run_download_leaf_page_adf() {
        let server = wiremock::MockServer::start().await;
        mock_leaf_page(&server, "12345").await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        let mut params = base_params(temp.path(), server.uri());
        params.format = ContentFormat::Adf;
        params.max_depth = 1;
        params.on_conflict = OnConflict::Skip;

        assert!(run_download(&api, &params).await.is_ok());
    }

    #[tokio::test]
    async fn run_download_resume_skip() {
        let server = wiremock::MockServer::start().await;
        mock_leaf_page(&server, "12345").await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();

        // Seed an existing manifest so resume takes the skip path.
        let mut manifest = BTreeMap::new();
        manifest.insert(
            "12345".to_string(),
            ManifestEntry {
                title: "Root Page".to_string(),
                path: "12345-root-page/index.md".to_string(),
                parent_id: None,
            },
        );
        let manifest_path = temp.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

        let mut params = base_params(temp.path(), server.uri());
        params.resume = true;

        assert!(run_download(&api, &params).await.is_ok());
    }

    #[tokio::test]
    async fn run_download_resume_moved_path_downloads() {
        let server = wiremock::MockServer::start().await;
        mock_leaf_page(&server, "12345").await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        // Pre-existing manifest has a different path for the same ID.
        let manifest = serde_json::json!({
            "12345": {"title": "Old Name", "path": "12345-old-name/index.md"}
        });
        std::fs::write(
            temp.path().join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let mut params = base_params(temp.path(), server.uri());
        params.resume = true;
        assert!(run_download(&api, &params).await.is_ok());

        assert!(temp.path().join("12345-root-page/index.md").exists());
    }

    #[tokio::test]
    async fn run_download_resume_new_page_downloads() {
        let server = wiremock::MockServer::start().await;
        mock_leaf_page(&server, "12345").await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        // Manifest exists but does not mention page "12345".
        let manifest = serde_json::json!({
            "999": {"title": "Other", "path": "999-other/index.md"}
        });
        std::fs::write(
            temp.path().join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let mut params = base_params(temp.path(), server.uri());
        params.resume = true;
        assert!(run_download(&api, &params).await.is_ok());

        assert!(temp.path().join("12345-root-page/index.md").exists());
    }

    #[tokio::test]
    async fn run_download_root_page_fetch_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        let mut params = base_params(temp.path(), server.uri());
        params.id = Some("99999".to_string());

        let err = run_download(&api, &params).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_download_child_fetch_failure_bails() {
        let server = wiremock::MockServer::start().await;
        // Page "100" — seeds successfully.
        let page_json = serde_json::json!({
            "id": "100",
            "title": "Root",
            "status": "current",
            "spaceId": "98765",
            "version": {"number": 1},
            "body": {
                "atlas_doc_format": {
                    "value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"
                }
            }
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/100"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&page_json))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;
        // Children of "100" returns a child whose get_content fails with 500.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/100/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "500", "title": "Broken"}]
                })),
            )
            .mount(&server)
            .await;
        // Child's get_content returns 500 — download_page fails.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/500"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("nope"))
            .mount(&server)
            .await;
        // Child's child listing returns empty so the loop terminates.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/500/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        let mut params = base_params(temp.path(), server.uri());
        params.id = Some("100".to_string());

        let err = run_download(&api, &params).await.unwrap_err();
        assert!(err.to_string().contains("failed to download"));
    }

    #[tokio::test]
    async fn run_download_from_space() {
        let server = wiremock::MockServer::start().await;

        // resolve_space_id
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        // list_space_root_pages — two roots
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "100", "title": "Alpha"},
                        {"id": "200", "title": "Beta"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        mock_leaf_page(&server, "100").await;
        mock_leaf_page(&server, "200").await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        let mut params = base_params(temp.path(), server.uri());
        params.id = None;
        params.space = Some("ENG".to_string());

        assert!(run_download(&api, &params).await.is_ok());
        // Directory slugs come from the titles returned by
        // list_space_root_pages (Alpha/Beta), not the get_content title.
        assert!(temp.path().join("100-alpha/index.md").exists());
        assert!(temp.path().join("200-beta/index.md").exists());
    }

    #[tokio::test]
    async fn run_download_empty_space_early_return() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        let mut params = base_params(temp.path(), server.uri());
        params.id = None;
        params.space = Some("EMPTY".to_string());

        assert!(run_download(&api, &params).await.is_ok());
        // No manifest written because we return before writing.
        assert!(!temp.path().join("manifest.json").exists());
    }

    #[tokio::test]
    async fn run_download_title_filter_skips_non_matching() {
        let server = wiremock::MockServer::start().await;

        // space → one root called "Welcome" with a child called "Architecture".
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "100", "title": "Welcome"}]
                })),
            )
            .mount(&server)
            .await;

        // children("100") = [{id: "200", title: "Architecture"}]
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/100/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "200", "title": "Architecture"}]
                })),
            )
            .mount(&server)
            .await;

        // Mock the "Architecture" page so the download succeeds.
        let page_json = serde_json::json!({
            "id": "200",
            "title": "Architecture",
            "status": "current",
            "spaceId": "98765",
            "version": {"number": 1},
            "parentId": "100",
            "body": {
                "atlas_doc_format": {
                    "value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"
                }
            }
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/200"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&page_json))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/200/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let api = build_arc_api(&server);
        let temp = tempfile::tempdir().unwrap();
        let mut params = base_params(temp.path(), server.uri());
        params.id = None;
        params.space = Some("ENG".to_string());
        params.title_filter = Some("architecture".to_string());

        assert!(run_download(&api, &params).await.is_ok());

        // Filtered-out root wrote no index.md.
        assert!(!temp.path().join("100-welcome/index.md").exists());
        // Matching child was downloaded under the filtered-out parent's dir.
        assert!(temp
            .path()
            .join("100-welcome/200-architecture/index.md")
            .exists());
    }
}
