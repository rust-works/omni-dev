//! Recursive, read-only Drive → local mirroring with an atomic ownership manifest.
//!
//! The destination is private to one sync root. Never delete content, follow local
//! symlinks, or replace a file not owned by the current manifest entry. Callers must
//! serialize runs and prevent concurrent local filesystem mutation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::drive::read::{
    resolve_export_mime_type, verify_sha256_checksum, GOOGLE_FOLDER, GOOGLE_SHORTCUT,
};
use crate::drive::client::DriveClient;
use crate::drive::files_api::{FilesApi, HARD_CAP, MAX_PAGE_LIMIT};
use crate::drive::types::DriveFile;

const MANIFEST: &str = ".omni-dev-sync.json";
const MAX_BYTES: u64 = 500 * 1024 * 1024;

/// Inputs to the one-way mirror. Dry runs perform no local writes or downloads.
pub(crate) struct SyncOptions {
    pub folder_id: String,
    pub dest: PathBuf,
    pub export_mime_type: Option<String>,
    pub verify: bool,
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    root_folder_id: String,
    // Includes folders to preserve their collision allocation across runs.
    files: BTreeMap<String, ManifestEntry>,
    #[serde(default)]
    orphan_paths: BTreeMap<PathBuf, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    rel_path: PathBuf,
    remote_name: String,
    mime_type: String,
    export_mime_type: Option<String>,
    modified_time: Option<String>,
    md5: Option<String>,
    sha256: Option<String>,
    size: Option<String>,
    #[serde(default)]
    pending: bool,
}

impl ManifestEntry {
    fn new(file: &DriveFile, path: PathBuf, export: Option<String>) -> Self {
        Self {
            rel_path: path,
            remote_name: file.name.clone(),
            mime_type: file.mime_type.clone(),
            export_mime_type: export,
            modified_time: file.modified_time.clone(),
            md5: file.md5_checksum.clone(),
            sha256: file.sha256_checksum.clone(),
            size: file.size.clone(),
            pending: false,
        }
    }
}

/// Serialized per-file result, including preserved orphan paths.
#[derive(Debug, Serialize)]
pub(crate) struct SyncItem {
    pub id: String,
    pub path: PathBuf,
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Complete outcome; counts describe planned actions when `dry_run` is true.
#[derive(Default, Debug, Serialize)]
pub(crate) struct SyncReport {
    pub dry_run: bool,
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub failed: usize,
    pub orphaned: usize,
    pub items: Vec<SyncItem>,
}

impl crate::cli::format::JsonlSerialize for SyncReport {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        crate::cli::format::write_scalar_jsonl(self, out)
    }
}

impl SyncReport {
    fn push(&mut self, id: &str, path: &Path, action: &'static str, error: Option<String>) {
        match action {
            "created" => self.created += 1,
            "updated" => self.updated += 1,
            "skipped" => self.skipped += 1,
            "failed" => self.failed += 1,
            "orphaned" => self.orphaned += 1,
            _ => unreachable!("internal action"), // patchcov: coverage ignore-line reason="every caller passes one of the five literal actions above; the arm only guards against a future caller adding a sixth without a counter"
        }
        self.items.push(SyncItem {
            id: id.to_string(),
            path: path.to_path_buf(),
            action,
            error,
        });
    }
}

struct RemoteEntry {
    file: DriveFile,
    parent_id: String,
}

/// Discover the entire tree before writing, so incomplete results never imply deletions.
async fn walk_folder(api: &FilesApi<'_>, root: &str) -> Result<Vec<RemoteEntry>> {
    ensure!(
        api.get_metadata(root).await?.mime_type == GOOGLE_FOLDER,
        "sync root must be a Drive folder"
    );
    let mut queue = VecDeque::from([root.to_string()]);
    let mut visited = BTreeSet::from([root.to_string()]);
    let mut entries = Vec::new();
    while let Some(parent_id) = queue.pop_front() {
        let escaped = parent_id.replace('\\', "\\\\").replace('\'', "\\'");
        let query = format!("'{escaped}' in parents and trashed = false");
        let mut list = list_children(api, &query, &parent_id).await?;
        list.files.sort_by(|a, b| a.id.cmp(&b.id));
        for file in list.files {
            if !visited.insert(file.id.clone()) {
                continue;
            }
            if file.mime_type == GOOGLE_FOLDER {
                queue.push_back(file.id.clone());
            }
            entries.push(RemoteEntry {
                file,
                parent_id: parent_id.clone(),
            });
        }
    }
    Ok(entries)
}

/// Check every page: search_all's final-page incompleteSearch is insufficient.
async fn list_children(
    api: &FilesApi<'_>,
    query: &str,
    folder: &str,
) -> Result<crate::drive::types::FileListResponse> {
    let mut result = crate::drive::types::FileListResponse::default();
    let mut token = None;
    let mut seen_tokens = BTreeSet::new();
    loop {
        let page = api
            .search(Some(query), MAX_PAGE_LIMIT, token.as_deref())
            .await?;
        ensure!(
            page.incomplete_search != Some(true),
            "folder {folder}: incomplete listing"
        );
        result.files.extend(page.files);
        ensure!(
            result.files.len() < HARD_CAP,
            "folder {folder}: {HARD_CAP}-file safety cap reached"
        );
        let Some(next) = page.next_page_token else {
            return Ok(result);
        };
        ensure!(
            !next.is_empty() && seen_tokens.insert(next.clone()),
            "folder {folder}: repeated or empty pagination token"
        );
        token = Some(next);
    }
}

/// Portable segments, with room left for an export extension and collision suffix.
fn sanitize_segment(name: &str) -> String {
    let mut out: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    out = out.trim_end_matches(['.', ' ']).to_string();
    while out.len() > 180 {
        out.pop();
    }
    out = out.trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() {
        out.push('_');
    }
    if is_windows_device(&out) {
        out.insert(0, '_');
    }
    out
}

fn is_windows_device(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/json" => "json",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        _ => "export",
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn validate_relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_)))
            && path.as_os_str() == path.components().collect::<PathBuf>().as_os_str(),
        "invalid manifest path: {}",
        path.display()
    );
    for component in path.components() {
        let name = component
            .as_os_str()
            .to_str()
            .context("non-UTF-8 manifest path")?;
        ensure!(
            !name.eq_ignore_ascii_case(MANIFEST)
                && !name.chars().any(|c| c.is_control()
                    || matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'))
                && !name.ends_with(['.', ' '])
                && name.len() <= 240
                && !is_windows_device(name),
            "unsafe manifest path: {}",
            path.display()
        );
    }
    Ok(())
}

/// Checks every component under dest without following a symlink, including the leaf.
fn safe_path(dest: &Path, rel: &Path) -> Result<PathBuf> {
    // The reserved manifest is checked by callers using this same component walk.
    ensure!(
        rel.components().all(|c| matches!(c, Component::Normal(_))),
        "unsafe relative path"
    );
    let mut path = dest.to_path_buf();
    for component in rel.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "refusing symlink: {}",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
        }
    }
    Ok(path)
}

fn load_manifest(opts: &SyncOptions) -> Result<Manifest> {
    let dest: PathBuf = opts.dest.components().collect();
    if let Ok(meta) = fs::symlink_metadata(&dest) {
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "destination must be a directory, not a symlink"
        );
    }
    let path = safe_path(&opts.dest, Path::new(MANIFEST))?;
    if path.exists() {
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(&path)?).context("invalid sync manifest")?;
        ensure!(
            manifest.version == 1,
            "unsupported sync manifest version {}",
            manifest.version
        );
        ensure!(
            manifest.root_folder_id == opts.folder_id,
            "manifest belongs to a different Drive folder; choose another --dest"
        );
        let mut paths = BTreeMap::new();
        for (id, entry) in &manifest.files {
            validate_relative(&entry.rel_path)?;
            ensure!(
                paths
                    .insert(path_key(&entry.rel_path), (id, entry))
                    .is_none(),
                "duplicate manifest path"
            );
        }
        let mut orphan_keys = BTreeSet::new();
        for path in manifest.orphan_paths.keys() {
            validate_relative(path)?;
            ensure!(
                !paths.contains_key(&path_key(path)) && orphan_keys.insert(path_key(path)),
                "orphan path overlaps active manifest path"
            );
        }
        for entry in manifest.files.values() {
            for ancestor in entry.rel_path.ancestors().skip(1) {
                if let Some((_, owner)) = paths.get(&path_key(ancestor)) {
                    ensure!(
                        owner.mime_type == GOOGLE_FOLDER,
                        "manifest file is a path ancestor"
                    );
                }
            }
        }
        Ok(manifest)
    } else {
        if opts.dest.exists() {
            ensure!(
                fs::read_dir(&opts.dest)?.next().is_none(),
                "destination is non-empty without a sync manifest; choose an empty --dest"
            );
        }
        Ok(Manifest {
            version: 1,
            root_folder_id: opts.folder_id.clone(),
            files: BTreeMap::new(),
            orphan_paths: BTreeMap::new(),
        })
    }
}

fn atomic_write(dest: &Path, rel: &Path, bytes: &[u8]) -> Result<()> {
    let path = safe_path(dest, rel)?;
    let parent = path.parent().context("missing output parent")?;
    fs::create_dir_all(parent)?;
    safe_path(dest, rel)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.persist(&path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn save_manifest(dest: &Path, manifest: &Manifest) -> Result<()> {
    atomic_write(
        dest,
        Path::new(MANIFEST),
        &serde_json::to_vec_pretty(manifest)?,
    )
}

/// Allocates a path without reassigning any historical file/directory ownership.
fn allocate_path(
    file: &DriveFile,
    parent: &Path,
    export: Option<&str>,
    old: Option<&ManifestEntry>,
    reserved: &mut BTreeSet<String>,
) -> PathBuf {
    if let Some(old) = old {
        if old.remote_name == file.name
            && old.rel_path.parent() == Some(parent)
            && old.mime_type == file.mime_type
            && old.export_mime_type.as_deref() == export
        {
            return old.rel_path.clone();
        }
    }
    let name = sanitize_segment(&file.name);
    let ext = export
        .map(|m| format!(".{}", extension(m)))
        .unwrap_or_default();
    let mut index = 0;
    loop {
        let suffix = if index == 0 {
            String::new()
        } else {
            format!(" ({index})")
        };
        let path = parent.join(format!("{name}{suffix}{ext}"));
        if reserved.insert(path_key(&path)) {
            return path;
        }
        index += 1;
    }
}

fn unchanged(file: &DriveFile, entry: &ManifestEntry, path: &Path, export: Option<&str>) -> bool {
    if entry.pending
        || entry.rel_path != path
        || entry.mime_type != file.mime_type
        || entry.export_mime_type.as_deref() != export
    {
        return false;
    }
    if !file.is_google_native() {
        if let Some(md5) = file.md5_checksum.as_ref().filter(|s| !s.is_empty()) {
            return entry.md5.as_ref() == Some(md5);
        }
    }
    file.modified_time
        .as_ref()
        .filter(|s| !s.is_empty())
        .is_some_and(|time| entry.modified_time.as_ref() == Some(time))
}

async fn sync_content(
    api: &FilesApi<'_>,
    opts: &SyncOptions,
    file: &DriveFile,
    rel: &Path,
    export: Option<&str>,
    old: Option<&ManifestEntry>,
    manifest: &mut Manifest,
) -> Result<&'static str> {
    let path = safe_path(&opts.dest, rel)?;
    if opts.verify && !file.is_google_native() {
        ensure!(
            file.sha256_checksum.as_ref().is_some_and(|s| !s.is_empty()),
            "--verify requires Drive's SHA-256 checksum for binary files"
        );
    }
    let exists = match fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "local path is not a regular file: {}",
                path.display()
            );
            ensure!(
                old.is_some_and(|e| e.rel_path == rel),
                "refusing to replace an unowned local file: {}",
                path.display()
            );
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()), // patchcov: coverage ignore-line reason="TOCTOU only: safe_path above already ran this identical lstat on the leaf and fails on any non-NotFound error, so a deterministic test cannot reach it"
    };
    if exists && old.is_some_and(|e| unchanged(file, e, rel, export)) {
        if opts.verify && !file.is_google_native() {
            ensure!(
                fs::metadata(&path)?.len() <= MAX_BYTES,
                "local file exceeds verification size cap"
            );
            verify_sha256_checksum(&fs::read(&path)?, file.sha256_checksum.as_deref())?;
        }
        return Ok("skipped");
    }
    ensure!(
        file.size
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .is_none_or(|s| s <= MAX_BYTES),
        "file exceeds 500 MiB download cap"
    );
    let action = if old.is_some_and(|e| !e.pending) {
        "updated"
    } else {
        "created"
    };
    if !opts.dry_run {
        let bytes = if let Some(mime) = export {
            api.export(&file.id, mime).await?
        } else {
            api.download(&file.id).await?
        };
        if opts.verify && !file.is_google_native() {
            verify_sha256_checksum(&bytes, file.sha256_checksum.as_deref())?;
        }
        // Claim only after download/verification succeed, but before replacing bytes.
        // A crash before the completion checkpoint leaves an explicitly retryable entry.
        let mut pending = ManifestEntry::new(file, rel.to_path_buf(), export.map(str::to_string));
        pending.pending = true;
        if let Some(old) = old.filter(|e| e.rel_path != rel) {
            manifest
                .orphan_paths
                .insert(old.rel_path.clone(), file.id.clone());
        }
        manifest.files.insert(file.id.clone(), pending);
        save_manifest(&opts.dest, manifest)?;
        atomic_write(&opts.dest, rel, &bytes)?;
    }
    Ok(action)
}

/// Mirrors a folder, preserving orphaned copies and checkpointing successful writes.
pub(crate) async fn run_sync(client: &DriveClient, opts: &SyncOptions) -> Result<SyncReport> {
    let mut manifest = load_manifest(opts)?;
    let api = FilesApi::new(client);
    let entries = walk_folder(&api, &opts.folder_id).await?;
    let mut report = SyncReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };
    let mut reserved: BTreeSet<String> = manifest
        .files
        .values()
        .map(|e| path_key(&e.rel_path))
        .collect();
    reserved.extend(manifest.orphan_paths.keys().map(|p| path_key(p)));
    reserved.insert(MANIFEST.to_string());
    let mut folders = BTreeMap::from([(opts.folder_id.clone(), PathBuf::new())]);
    let seen: BTreeSet<_> = entries.iter().map(|e| e.file.id.clone()).collect();
    let previous = manifest.files.clone();
    if !opts.dry_run {
        fs::create_dir_all(&opts.dest)?;
        save_manifest(&opts.dest, &manifest)?;
    }
    for remote in entries {
        let file = remote.file;
        let parent = folders
            .get(&remote.parent_id)
            .context("missing parent allocation")?;
        let old = previous.get(&file.id);
        if file.mime_type == GOOGLE_SHORTCUT {
            report.push(
                &file.id,
                &parent.join(sanitize_segment(&file.name)),
                "skipped",
                None,
            );
            continue;
        }
        let export = if file.is_google_native() && file.mime_type != GOOGLE_FOLDER {
            match resolve_export_mime_type(&file, opts.export_mime_type.as_deref()) {
                Ok(mime) => Some(mime),
                Err(e) => {
                    report.push(
                        &file.id,
                        &parent.join(sanitize_segment(&file.name)),
                        "failed",
                        Some(e.to_string()),
                    );
                    continue;
                }
            }
        } else {
            None
        };
        let rel = allocate_path(&file, parent, export.as_deref(), old, &mut reserved);
        if file.mime_type == GOOGLE_FOLDER {
            // Propagate unsafe directory failures to all descendant files through safe_path.
            folders.insert(file.id.clone(), rel.clone());
            let result = safe_path(&opts.dest, &rel).and_then(|path| {
                if opts.dry_run {
                    if path.exists() {
                        ensure!(path.is_dir(), "local folder path is not a directory");
                    }
                } else {
                    fs::create_dir_all(path)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                report.push(&file.id, &rel, "failed", Some(e.to_string()));
                continue;
            }
        } else {
            match sync_content(
                &api,
                opts,
                &file,
                &rel,
                export.as_deref(),
                old,
                &mut manifest,
            )
            .await
            {
                Ok(action) => report.push(&file.id, &rel, action, None),
                Err(e) => {
                    report.push(&file.id, &rel, "failed", Some(format!("{e:#}")));
                    continue;
                }
            }
        }
        if let Some(old) = old.filter(|e| e.rel_path != rel) {
            manifest
                .orphan_paths
                .insert(old.rel_path.clone(), file.id.clone());
        }
        manifest
            .files
            .insert(file.id.clone(), ManifestEntry::new(&file, rel, export));
        if !opts.dry_run {
            save_manifest(&opts.dest, &manifest)?;
        }
    }
    for (id, entry) in &previous {
        if !seen.contains(id) {
            report.push(id, &entry.rel_path, "orphaned", None);
        }
    }
    for (path, id) in &manifest.orphan_paths {
        report.push(id, path, "orphaned", None);
    }
    if !opts.dry_run {
        save_manifest(&opts.dest, &manifest)?;
    }
    Ok(report)
}

#[cfg(test)]
pub(crate) mod tests;
