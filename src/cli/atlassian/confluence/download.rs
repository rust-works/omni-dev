//! CLI command for recursively downloading Confluence page trees.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use crate::atlassian::api::AtlassianApi;
use crate::atlassian::confluence_api::ConfluenceApi;
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
#[derive(Parser)]
pub struct DownloadCommand {
    /// Root Confluence page ID to start from.
    pub id: String,

    /// Output directory.
    #[arg(long, default_value = ".")]
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
    id: String,
    output_dir: PathBuf,
    format: ContentFormat,
    concurrency: usize,
    max_depth: u32,
    resume: bool,
    on_conflict: OnConflict,
    instance_url: String,
}

impl DownloadCommand {
    /// Executes the recursive download.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = Arc::new(ConfluenceApi::new(client));
        let params = DownloadParams {
            id: self.id,
            output_dir: self.output_dir,
            format: self.format,
            concurrency: self.concurrency,
            max_depth: self.max_depth,
            resume: self.resume,
            on_conflict: self.on_conflict,
            instance_url,
        };
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

    // Fetch root page to get its title
    eprintln!("Fetching root page {}...", params.id);
    let root = api.get_content(&params.id).await?;
    let root_slug = slugify(&root.title);
    let root_dir = params.output_dir.join(format!("{}-{}", root.id, root_slug));

    let root_parent = match &root.metadata {
        crate::atlassian::api::ContentMetadata::Confluence { parent_id, .. } => parent_id.clone(),
        crate::atlassian::api::ContentMetadata::Jira { .. } => None,
    };

    let mut queue: VecDeque<PageTask> = VecDeque::new();
    queue.push_back(PageTask {
        id: root.id.clone(),
        title: root.title.clone(),
        dir: root_dir,
        depth: 0,
        parent_id: root_parent,
    });

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    while let Some(task) = queue.pop_front() {
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

        // Fetch children and enqueue (unless at max depth)
        if params.max_depth > 0 && task.depth >= params.max_depth {
            continue;
        }

        match api.get_children(&task.id).await {
            Ok(children) => {
                for child in children {
                    let child_slug = slugify(&child.title);
                    let child_dir = task.dir.join(format!("{}-{}", child.id, child_slug));
                    queue.push_back(PageTask {
                        id: child.id,
                        title: child.title,
                        dir: child_dir,
                        depth: task.depth + 1,
                        parent_id: Some(task.id.clone()),
                    });
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
            id: "12345".to_string(),
            output_dir: PathBuf::from("."),
            format: ContentFormat::Jfm,
            concurrency: 8,
            max_depth: 0,
            resume: false,
            on_conflict: OnConflict::Backup,
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.concurrency, 8);
        assert!(!cmd.resume);
        assert!(matches!(cmd.on_conflict, OnConflict::Backup));
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
}
