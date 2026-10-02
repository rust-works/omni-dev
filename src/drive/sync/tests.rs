//! Mocked Drive and filesystem regression tests; never access real accounts.
#![allow(clippy::unwrap_used)]

use super::*;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub(crate) async fn fixture() -> (MockServer, DriveClient) {
    let server = MockServer::start().await;
    let client = crate::drive::test_support::client_with_bootstrapped_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/root"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"root","name":"root","mimeType":GOOGLE_FOLDER})),
        )
        .mount(&server)
        .await;
    (server, client)
}

pub(crate) async fn list(server: &MockServer, folder: &str, files: Value) {
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param(
            "q",
            format!("'{folder}' in parents and trashed = false"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files":files})))
        .mount(server)
        .await;
}

pub(crate) fn options(dest: &Path) -> SyncOptions {
    SyncOptions {
        folder_id: "root".into(),
        dest: dest.into(),
        export_mime_type: None,
        verify: false,
        dry_run: false,
    }
}

fn binary(id: &str, name: &str, hash: &str) -> Value {
    json!({"id":id,"name":name,"mimeType":"text/plain","md5Checksum":hash,"modifiedTime":"time"})
}

async fn download(server: &MockServer, id: &str, body: &str, count: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/drive/v3/files/{id}")))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(count)
        .mount(server)
        .await;
}

#[test]
fn names_are_portable_bounded_and_cannot_traverse() {
    for (raw, expected) in [
        ("../../etc/passwd", ".._.._etc_passwd"),
        ("a\\b\0\n", "a_b"),
        ("..", "_"),
        (".", "_"),
        ("", "_"),
        ("NUL.txt", "_NUL.txt"),
        ("COM1", "_COM1"),
        ("a:*. ", "a__"),
    ] {
        assert_eq!(sanitize_segment(raw), expected);
        validate_relative(Path::new(expected)).unwrap();
    }
    let long = sanitize_segment(&"é".repeat(200));
    assert!(long.len() <= 180);
    validate_relative(Path::new(&(long + ".md"))).unwrap();
    for bad in [
        "../evil",
        "/evil",
        ".omni-dev-sync.json",
        "foo/.omni-dev-sync.json",
        "a\\b",
        "a/../b",
    ] {
        assert!(validate_relative(Path::new(bad)).is_err(), "{bad}");
    }
}

#[test]
fn collision_allocation_reserves_case_insensitive_paths_and_manifest() {
    let mut reserved = BTreeSet::from([
        MANIFEST.to_string(),
        "a.md".to_string(),
        "a (1).md".to_string(),
    ]);
    let file = DriveFile {
        name: "A".into(),
        ..Default::default()
    };
    assert_eq!(
        allocate_path(
            &file,
            Path::new(""),
            Some("text/markdown"),
            None,
            &mut reserved
        ),
        Path::new("A (2).md")
    );
    let file = DriveFile {
        name: MANIFEST.into(),
        ..Default::default()
    };
    assert_ne!(
        allocate_path(&file, Path::new(""), None, None, &mut reserved),
        Path::new(MANIFEST)
    );
}

#[tokio::test]
async fn recursive_first_run_checkpoints_then_skips_without_downloads() {
    let (server, client) = fixture().await;
    list(&server, "root", json!([{"id":"dir","name":"nested","mimeType":GOOGLE_FOLDER},binary("b","A.txt","hash"),binary("a","a.txt","hash")])).await;
    list(&server, "dir", json!([binary("child","child.txt","hash"),{"id":"root","name":"cycle","mimeType":GOOGLE_FOLDER}])).await;
    for id in ["a", "b", "child"] {
        download(&server, id, "hello", 1).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let opts = options(dir.path());
    let first = run_sync(&client, &opts).await.unwrap();
    assert_eq!(first.created, 3);
    assert_eq!(
        fs::read(dir.path().join("nested/child.txt")).unwrap(),
        b"hello"
    );
    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"hello");
    assert_eq!(fs::read(dir.path().join("A.txt (1)")).unwrap(), b"hello");
    let second = run_sync(&client, &opts).await.unwrap();
    assert_eq!(second.skipped, 3);
    assert_eq!(second.failed, 0);
    assert!(dir.path().join(MANIFEST).exists());
}

#[tokio::test]
async fn updates_missing_and_changed_files_and_preserves_orphans() {
    let (server, client) = fixture().await;
    list(
        &server,
        "root",
        json!([binary("a", "a", "one"), binary("b", "b", "one")]),
    )
    .await;
    download(&server, "a", "hello", 1).await;
    download(&server, "b", "hello", 1).await;
    let dir = tempfile::tempdir().unwrap();
    let opts = options(dir.path());
    run_sync(&client, &opts).await.unwrap();
    server.reset().await;
    let client = crate::drive::test_support::client_with_bootstrapped_token(&server).await;
    root(&server).await;
    list(&server, "root", json!([binary("a", "a", "two")])).await;
    download(&server, "a", "changed", 2).await;
    let report = run_sync(&client, &opts).await.unwrap();
    assert_eq!((report.updated, report.orphaned), (1, 1));
    assert_eq!(fs::read(dir.path().join("b")).unwrap(), b"hello");
    fs::remove_file(dir.path().join("a")).unwrap();
    assert_eq!(run_sync(&client, &opts).await.unwrap().updated, 1);
}

async fn root(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/root"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"root","name":"root","mimeType":GOOGLE_FOLDER})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn renamed_copies_remain_reserved_on_later_runs() {
    let (server, client) = fixture().await;
    list(&server, "root", json!([binary("a", "old", "hash")])).await;
    download(&server, "a", "first", 1).await;
    let dir = tempfile::tempdir().unwrap();
    let opts = options(dir.path());
    run_sync(&client, &opts).await.unwrap();
    server.reset().await;
    root(&server).await;
    list(&server, "root", json!([binary("a", "new", "hash")])).await;
    download(&server, "a", "renamed", 1).await;
    assert_eq!(run_sync(&client, &opts).await.unwrap().orphaned, 1);
    server.reset().await;
    root(&server).await;
    list(
        &server,
        "root",
        json!([binary("a", "new", "hash"), binary("b", "old", "hash")]),
    )
    .await;
    download(&server, "b", "second", 1).await;
    let report = run_sync(&client, &opts).await.unwrap();
    assert_eq!((report.created, report.skipped, report.orphaned), (1, 1, 1));
    assert_eq!(fs::read(dir.path().join("old")).unwrap(), b"first");
    assert_eq!(fs::read(dir.path().join("old (1)")).unwrap(), b"second");
}

#[tokio::test]
async fn native_exports_shortcuts_and_partial_failures_are_reported() {
    let (server, client) = fixture().await;
    list(&server, "root", json!([
        {"id":"doc","name":"notes","mimeType":"application/vnd.google-apps.document","modifiedTime":"one"},
        {"id":"shortcut","name":"link","mimeType":GOOGLE_SHORTCUT},
        {"id":"form","name":"form","mimeType":"application/vnd.google-apps.form"},
        binary("missing","missing","hash"), binary("ok","ok","hash")
    ])).await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/doc/export"))
        .and(query_param("mimeType", "text/markdown"))
        .respond_with(ResponseTemplate::new(200).set_body_string("# Notes"))
        .expect(1)
        .mount(&server)
        .await;
    download(&server, "ok", "hello", 1).await;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    let report = run_sync(&client, &opts).await.unwrap();
    assert_eq!((report.created, report.skipped, report.failed), (2, 1, 2));
    assert_eq!(fs::read(dir.path().join("notes.md")).unwrap(), b"# Notes");
    opts.verify = true;
    // Native export skips even with --verify; binaries fail if no checksum exists.
    let report = run_sync(&client, &opts).await.unwrap();
    assert_eq!(report.skipped, 2);
    assert_eq!(report.failed, 3);
}

#[tokio::test]
async fn explicit_export_changes_path_and_refreshes_content() {
    let (server, client) = fixture().await;
    list(&server, "root", json!([{"id":"doc","name":"notes","mimeType":"application/vnd.google-apps.document","modifiedTime":"one"}])).await;
    for (mime, content) in [("text/markdown", "markdown"), ("application/pdf", "pdf")] {
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/doc/export"))
            .and(query_param("mimeType", mime))
            .respond_with(ResponseTemplate::new(200).set_body_string(content))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    run_sync(&client, &opts).await.unwrap();
    opts.export_mime_type = Some("application/pdf".into());
    let report = run_sync(&client, &opts).await.unwrap();
    assert_eq!((report.updated, report.orphaned), (1, 1));
    assert_eq!(fs::read(dir.path().join("notes.pdf")).unwrap(), b"pdf");
    assert!(dir.path().join("notes.md").exists());
}

#[tokio::test]
async fn verification_checks_downloads_and_existing_local_bytes() {
    let (server, client) = fixture().await;
    let mut file = binary("a", "a", "hash");
    file["sha256Checksum"] =
        json!("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    list(&server, "root", json!([file])).await;
    download(&server, "a", "hello", 1).await;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    opts.verify = true;
    assert_eq!(run_sync(&client, &opts).await.unwrap().created, 1);
    assert_eq!(run_sync(&client, &opts).await.unwrap().skipped, 1);
    fs::write(dir.path().join("a"), "edited").unwrap();
    assert_eq!(run_sync(&client, &opts).await.unwrap().failed, 1);
    assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"edited");
}

#[tokio::test]
async fn dry_run_does_not_create_destination_or_download() {
    let (server, client) = fixture().await;
    list(
        &server,
        "root",
        json!([binary("a","a","hash"),{"id":"dir","name":"nested","mimeType":GOOGLE_FOLDER}]),
    )
    .await;
    list(&server, "dir", json!([binary("b", "b", "hash")])).await;
    download(&server, "a", "hello", 0).await;
    download(&server, "b", "hello", 0).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("absent");
    let mut opts = options(&dest);
    opts.dry_run = true;
    assert_eq!(run_sync(&client, &opts).await.unwrap().created, 2);
    assert!(!dest.exists());
    opts.verify = true;
    assert_eq!(run_sync(&client, &opts).await.unwrap().failed, 2);
    assert!(!dest.exists());
}

#[test]
fn manifest_rejects_unmanaged_roots_versions_and_malicious_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    let mut manifest = load_manifest(&opts).unwrap();
    save_manifest(dir.path(), &manifest).unwrap();
    opts.folder_id = "other".into();
    assert!(load_manifest(&opts)
        .unwrap_err()
        .to_string()
        .contains("different"));
    opts.folder_id = "root".into();
    manifest.version = 2;
    save_manifest(dir.path(), &manifest).unwrap();
    assert!(load_manifest(&opts).is_err());
    manifest.version = 1;
    let file = DriveFile::default();
    manifest.files.insert(
        "a".into(),
        ManifestEntry::new(&file, "../escape".into(), None),
    );
    save_manifest(dir.path(), &manifest).unwrap();
    assert!(load_manifest(&opts).is_err());
    manifest.files.clear();
    for (id, path) in [("a", "Same"), ("b", "same")] {
        manifest
            .files
            .insert(id.into(), ManifestEntry::new(&file, path.into(), None));
    }
    save_manifest(dir.path(), &manifest).unwrap();
    assert!(load_manifest(&opts).is_err());
    fs::remove_file(dir.path().join(MANIFEST)).unwrap();
    fs::write(dir.path().join("unrelated"), "keep").unwrap();
    assert!(load_manifest(&opts)
        .unwrap_err()
        .to_string()
        .contains("non-empty"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_and_unowned_local_files_are_never_overwritten() {
    use std::os::unix::fs::symlink;
    let (server, client) = fixture().await;
    list(&server, "root", json!([binary("a", "a", "hash")])).await;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("target"), "keep").unwrap();
    let opts = options(dir.path());
    let manifest = load_manifest(&opts).unwrap();
    save_manifest(dir.path(), &manifest).unwrap();
    symlink(outside.path().join("target"), dir.path().join("a")).unwrap();
    assert_eq!(run_sync(&client, &opts).await.unwrap().failed, 1);
    assert_eq!(fs::read(outside.path().join("target")).unwrap(), b"keep");
    fs::remove_file(dir.path().join("a")).unwrap();
    fs::write(dir.path().join("a"), "unowned").unwrap();
    assert_eq!(run_sync(&client, &opts).await.unwrap().failed, 1);
    assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"unowned");
    fs::remove_file(dir.path().join(MANIFEST)).unwrap();
    symlink(outside.path().join("target"), dir.path().join(MANIFEST)).unwrap();
    assert!(load_manifest(&opts).is_err());
    assert!(safe_path(dir.path(), Path::new(MANIFEST)).is_err());
}

#[test]
fn change_detection_requires_real_markers_and_checks_native_time() {
    let mut file = DriveFile {
        name: "name".into(),
        mime_type: "text/plain".into(),
        ..Default::default()
    };
    let mut entry = ManifestEntry::new(&file, "name".into(), None);
    assert!(!unchanged(&file, &entry, Path::new("name"), None));
    file.modified_time = Some("one".into());
    entry.modified_time = file.modified_time.clone();
    assert!(unchanged(&file, &entry, Path::new("name"), None));
    file.mime_type = "application/vnd.google-apps.document".into();
    entry = ManifestEntry::new(&file, "name.md".into(), Some("text/markdown".into()));
    assert!(unchanged(
        &file,
        &entry,
        Path::new("name.md"),
        Some("text/markdown")
    ));
    file.modified_time = Some("two".into());
    assert!(!unchanged(
        &file,
        &entry,
        Path::new("name.md"),
        Some("text/markdown")
    ));
}

#[tokio::test]
async fn incomplete_listing_and_api_errors_abort_before_writing() {
    for response in [
        ResponseTemplate::new(200).set_body_json(json!({"files":[],"incompleteSearch":true})),
        ResponseTemplate::new(403),
    ] {
        let (server, client) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(response)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("absent");
        assert!(run_sync(&client, &options(&dest)).await.is_err());
        assert!(!dest.exists());
    }
}

#[tokio::test]
async fn pagination_is_complete_and_shared_folders_are_visited_once() {
    let (server, client) = fixture().await;
    let folder = json!({"id":"shared","name":"shared","mimeType":GOOGLE_FOLDER});
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("q", "'root' in parents and trashed = false"))
        .and(query_param("pageToken", "next"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files":[folder.clone()]})))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/drive/v3/files"))
        .and(query_param("q", "'root' in parents and trashed = false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files":[{"id":"other","name":"other","mimeType":GOOGLE_FOLDER}],"nextPageToken":"next"})))
        .with_priority(2).expect(1).mount(&server).await;
    list(&server, "other", json!([folder])).await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("q", "'shared' in parents and trashed = false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files":[]})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        run_sync(&client, &options(dir.path()))
            .await
            .unwrap()
            .failed,
        0
    );
    assert!(dir.path().join("shared").is_dir());
    assert!(!dir.path().join("other/shared").exists());
}

#[tokio::test]
async fn exact_listing_cap_is_rejected_and_non_folder_roots_fail() {
    let (server, client) = fixture().await;
    let files = vec![binary("a", "a", "hash"); HARD_CAP];
    list(&server, "root", json!(files)).await;
    let dir = tempfile::tempdir().unwrap();
    assert!(run_sync(&client, &options(dir.path()))
        .await
        .unwrap_err()
        .to_string()
        .contains("safety cap"));
    assert!(!dir.path().join(MANIFEST).exists());
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/root"))
        .respond_with(ResponseTemplate::new(200).set_body_json(binary("root", "a", "hash")))
        .mount(&server)
        .await;
    assert!(run_sync(&client, &options(dir.path()))
        .await
        .unwrap_err()
        .to_string()
        .contains("folder"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_directory_and_destination_never_receive_writes() {
    use std::os::unix::fs::symlink;
    let (server, client) = fixture().await;
    list(
        &server,
        "root",
        json!([{"id":"dir","name":"nested","mimeType":GOOGLE_FOLDER}]),
    )
    .await;
    list(&server, "dir", json!([binary("a", "a", "hash")])).await;
    download(&server, "a", "hello", 0).await;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let opts = options(dir.path());
    save_manifest(dir.path(), &load_manifest(&opts).unwrap()).unwrap();
    symlink(outside.path(), dir.path().join("nested")).unwrap();
    assert_eq!(run_sync(&client, &opts).await.unwrap().failed, 2);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    symlink(outside.path(), dir.path().join("dest")).unwrap();
    assert!(run_sync(&client, &options(&dir.path().join("dest")))
        .await
        .is_err());
    assert!(load_manifest(&options(&PathBuf::from(format!(
        "{}/dest/",
        dir.path().display()
    ))))
    .is_err());
}

#[tokio::test]
async fn checksum_failure_does_not_write_content_or_claim_ownership() {
    let (server, client) = fixture().await;
    let mut file = binary("a", "a", "hash");
    file["sha256Checksum"] = json!("wrong");
    list(&server, "root", json!([file])).await;
    download(&server, "a", "hello", 1).await;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    opts.verify = true;
    assert_eq!(run_sync(&client, &opts).await.unwrap().failed, 1);
    assert!(!dir.path().join("a").exists());
    assert!(load_manifest(&opts).unwrap().files.is_empty());
}

#[tokio::test]
async fn interrupted_pending_writes_are_redownloaded_and_completed() {
    let (server, client) = fixture().await;
    list(&server, "root", json!([binary("a", "a", "hash")])).await;
    download(&server, "a", "complete", 2).await;
    let dir = tempfile::tempdir().unwrap();
    let opts = options(dir.path());
    // Simulate interruption after the pending checkpoint and content replacement,
    // before the completion checkpoint. The remote marker already matches.
    let file: DriveFile = serde_json::from_value(binary("a", "a", "hash")).unwrap();
    let mut manifest = load_manifest(&opts).unwrap();
    let mut entry = ManifestEntry::new(&file, "a".into(), None);
    entry.pending = true;
    manifest.files.insert("a".into(), entry);
    save_manifest(dir.path(), &manifest).unwrap();
    fs::write(dir.path().join("a"), "interrupted").unwrap();
    let report = run_sync(&client, &opts).await.unwrap();
    assert_eq!((report.created, report.skipped, report.failed), (1, 0, 0));
    assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"complete");
    assert!(!load_manifest(&opts).unwrap().files["a"].pending);
    // Also recover interruption before content replacement (no local file).
    let mut manifest = load_manifest(&opts).unwrap();
    manifest.files.get_mut("a").unwrap().pending = true;
    save_manifest(dir.path(), &manifest).unwrap();
    fs::remove_file(dir.path().join("a")).unwrap();
    assert_eq!(run_sync(&client, &opts).await.unwrap().created, 1);
}

#[tokio::test]
async fn every_page_is_checked_and_repeated_tokens_abort_discovery() {
    for incomplete in [true, false] {
        let (server, client) = fixture().await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"files":[],"incompleteSearch":incomplete,"nextPageToken":"repeated"}),
            ))
            .expect(if incomplete { 1 } else { 2 })
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let error = run_sync(&client, &options(dir.path())).await.unwrap_err();
        assert!(error.to_string().contains(if incomplete {
            "incomplete"
        } else {
            "pagination token"
        }));
        assert!(!dir.path().join(MANIFEST).exists());
    }
}

#[test]
fn manifest_rejects_path_aliases_and_device_names() {
    for name in ["a/./b", "a//b", "a/", "NUL", "COM1.txt"] {
        assert!(validate_relative(Path::new(name)).is_err(), "{name}");
    }
    let boundary = format!("{}.rest", "a".repeat(179));
    let name = sanitize_segment(&boundary);
    assert!(!name.ends_with('.'));
    validate_relative(Path::new(&name)).unwrap();
}

#[test]
fn export_extensions_cover_every_known_type_and_default_to_export() {
    for (mime, ext) in [
        ("text/markdown", "md"),
        ("text/csv", "csv"),
        ("text/plain", "txt"),
        ("text/html", "html"),
        ("application/pdf", "pdf"),
        ("application/zip", "zip"),
        ("application/json", "json"),
        (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
        ),
        (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
        ),
        (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "pptx",
        ),
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/svg+xml", "svg"),
        ("application/x-unknown", "export"),
    ] {
        assert_eq!(extension(mime), ext, "{mime}");
    }
}

#[test]
fn safe_path_reports_inspection_failures_other_than_not_found() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "x").unwrap();
    // A file used as a directory fails with ENOTDIR, which is not NotFound.
    let err = safe_path(dir.path(), Path::new("file/child")).unwrap_err();
    assert!(err.to_string().starts_with("inspect "), "{err:#}");
}

#[tokio::test]
async fn a_directory_in_a_files_place_fails_only_that_file() {
    let (server, client) = fixture().await;
    list(&server, "root", json!([binary("a", "a", "hash")])).await;
    download(&server, "a", "hello", 0).await;
    let dir = tempfile::tempdir().unwrap();
    let opts = options(dir.path());
    save_manifest(dir.path(), &load_manifest(&opts).unwrap()).unwrap();
    fs::create_dir(dir.path().join("a")).unwrap();
    let report = run_sync(&client, &opts).await.unwrap();
    assert_eq!(report.failed, 1);
    assert!(report.items[0]
        .error
        .as_deref()
        .unwrap()
        .contains("not a regular file"));
    assert!(dir.path().join("a").is_dir());
}

#[tokio::test]
async fn dry_run_flags_a_local_file_where_a_folder_belongs() {
    let (server, client) = fixture().await;
    list(
        &server,
        "root",
        json!([{"id":"dir","name":"nested","mimeType":GOOGLE_FOLDER}]),
    )
    .await;
    list(&server, "dir", json!([binary("b", "b", "hash")])).await;
    download(&server, "b", "hello", 0).await;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    save_manifest(dir.path(), &load_manifest(&opts).unwrap()).unwrap();
    fs::write(dir.path().join("nested"), "not a folder").unwrap();
    opts.dry_run = true;
    let report = run_sync(&client, &opts).await.unwrap();
    // The blocked folder fails, and so does the file that would land beneath it.
    assert_eq!(report.failed, 2);
    assert!(report.items[0]
        .error
        .as_deref()
        .unwrap()
        .contains("not a directory"));
    assert_eq!(
        fs::read(dir.path().join("nested")).unwrap(),
        b"not a folder"
    );
}
