//! Wire types for the Drive v3 REST API.
//!
//! Field naming follows Drive's camelCase JSON via per-field
//! `#[serde(rename = "...")]`, mirroring `src/gmail/types.rs`. `size` is
//! Drive's own wire format: a **decimal string**, not a JSON number —
//! present only for binary files with actual byte content; absent for
//! folders and Google-native documents (Docs/Sheets/Slides/...), which have
//! no fixed byte size.

use std::collections::HashMap;
use std::io::Write;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};

/// An owner of a Drive file, as embedded in `files.list`/`files.get`'s
/// `owners[]` field (requested via the `fields` param's
/// `owners(displayName,emailAddress)` sub-selector — see `files_api.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Owner {
    /// The owner's display name.
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
    /// The owner's email address.
    #[serde(default, rename = "emailAddress")]
    pub email_address: Option<String>,
}

/// A Drive file (or folder, or Google-native document) — the `files`
/// resource returned by `files.list`/`files.get`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DriveFile {
    /// Drive file id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// MIME type. `application/vnd.google-apps.*` marks a Google-native
    /// document (Docs/Sheets/Slides/...) with no fixed byte content.
    #[serde(default, rename = "mimeType")]
    pub mime_type: String,
    /// Size in bytes, as a decimal string. Absent for folders and
    /// Google-native documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Last modification time (RFC 3339).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "modifiedTime"
    )]
    pub modified_time: Option<String>,
    /// Ids of the parent folders containing this file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
    /// A link for opening this file in a relevant Google editor or viewer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "webViewLink"
    )]
    pub web_view_link: Option<String>,
    /// The file's owners.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<Owner>,
    /// Id of the shared drive this file lives on, if any.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "driveId")]
    pub drive_id: Option<String>,
    /// Export links for a Google-native document, keyed by export MIME
    /// type. Present only on `files.get` (requested in `GET_FIELDS`;
    /// deliberately **not** requested by `files.list`'s `LIST_FIELDS` — it's
    /// irrelevant to a search-result table and would bloat every list
    /// response). Values are export URLs (unused — Drive's `files.export`
    /// endpoint is called directly instead); `drive read`'s content-export
    /// error path lists these keys when a Google-native file has no default
    /// export MIME type.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "exportLinks"
    )]
    pub export_links: Option<HashMap<String, String>>,
}

impl DriveFile {
    /// Whether this is a Google-native document (Docs/Sheets/Slides/Forms/
    /// Drawings/...) with no fixed byte content — must be fetched via
    /// `files.export`, never `files.get?alt=media`.
    #[must_use]
    pub fn is_google_native(&self) -> bool {
        self.mime_type.starts_with("application/vnd.google-apps.")
    }
}

/// Response envelope for `GET /drive/v3/files`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileListResponse {
    /// Matching files on this page.
    #[serde(default)]
    pub files: Vec<DriveFile>,
    /// Cursor for the next page, when more results are available.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "nextPageToken"
    )]
    pub next_page_token: Option<String>,
    /// Whether the search process was incomplete (partial results returned
    /// due to a transient issue on Google's side).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "incompleteSearch"
    )]
    pub incomplete_search: Option<bool>,
}

impl JsonlSerialize for DriveFile {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn file_list_response_deserializes_a_realistic_fixture() {
        let json = serde_json::json!({
            "files": [
                {
                    "id": "f1",
                    "name": "report.pdf",
                    "mimeType": "application/pdf",
                    "size": "12345",
                    "modifiedTime": "2026-01-01T00:00:00.000Z",
                    "parents": ["folder1"],
                    "webViewLink": "https://drive.google.com/file/d/f1/view",
                    "owners": [{"displayName": "Alice", "emailAddress": "alice@example.com"}],
                    "driveId": "shared1",
                },
            ],
            "nextPageToken": "page2",
            "incompleteSearch": false,
        });
        let response: FileListResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.files.len(), 1);
        let file = &response.files[0];
        assert_eq!(file.id, "f1");
        assert_eq!(file.size.as_deref(), Some("12345"));
        assert_eq!(
            file.owners[0].email_address.as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(file.drive_id.as_deref(), Some("shared1"));
        assert_eq!(response.next_page_token.as_deref(), Some("page2"));
    }

    #[test]
    fn size_round_trips_as_a_string_not_a_number() {
        let file = DriveFile {
            id: "f1".to_string(),
            name: "n".to_string(),
            size: Some("999".to_string()),
            ..Default::default()
        };
        let value = serde_json::to_value(&file).unwrap();
        assert_eq!(value["size"], serde_json::json!("999"));
    }

    #[test]
    fn is_google_native_true_for_google_apps_mime_type() {
        let file = DriveFile {
            mime_type: "application/vnd.google-apps.document".to_string(),
            ..Default::default()
        };
        assert!(file.is_google_native());
    }

    #[test]
    fn is_google_native_false_for_ordinary_mime_type() {
        let file = DriveFile {
            mime_type: "application/pdf".to_string(),
            ..Default::default()
        };
        assert!(!file.is_google_native());
    }

    #[test]
    fn drive_file_deserializes_from_minimal_fields() {
        let json = serde_json::json!({"id": "f1", "name": "n"});
        let file: DriveFile = serde_json::from_value(json).unwrap();
        assert_eq!(file.id, "f1");
        assert_eq!(file.mime_type, "");
        assert!(file.size.is_none());
        assert!(file.export_links.is_none());
    }

    #[test]
    fn deserializing_unmodeled_extra_field_still_succeeds() {
        let json = serde_json::json!({
            "id": "f1",
            "name": "n",
            "somethingNew": {"nested": true},
        });
        let file: DriveFile = serde_json::from_value(json).unwrap();
        assert_eq!(file.id, "f1");
    }

    #[test]
    fn export_links_parses_google_native_export_map() {
        let json = serde_json::json!({
            "id": "f1",
            "name": "doc",
            "mimeType": "application/vnd.google-apps.document",
            "exportLinks": {
                "text/markdown": "https://export.example/md",
                "application/pdf": "https://export.example/pdf",
            },
        });
        let file: DriveFile = serde_json::from_value(json).unwrap();
        let links = file.export_links.unwrap();
        assert_eq!(links.len(), 2);
        assert!(links.contains_key("text/markdown"));
    }

    #[test]
    fn drive_file_write_jsonl_emits_exactly_one_line() {
        let file = DriveFile {
            id: "f1".to_string(),
            name: "n".to_string(),
            ..Default::default()
        };
        let mut buf = Vec::new();
        file.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("f1"));
    }
}
