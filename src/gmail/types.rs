//! Wire types for the Gmail v1 REST API.
//!
//! Field naming follows Gmail's camelCase JSON via per-field
//! `#[serde(rename = "...")]` (matching the Jira precedent for a
//! camelCase upstream API, not Datadog's snake_case-native one).
//! `Message::payload`/`raw` are the MIME-tree escape hatch: the per-part
//! MIME structure is deeply recursive and heterogeneous, so it round-trips
//! as raw `serde_json::Value` rather than being modelled — the same
//! precedent as `Dashboard.widgets` in `src/datadog/types.rs`.

use std::io::Write;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::gmail::format::{write_items_jsonl, write_scalar_jsonl, JsonlSerialize};

/// A bare `(id, threadId)` pair, as returned by `messages.list` without
/// fetching each message's full content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MessageRef {
    /// Gmail message id.
    pub id: String,
    /// Id of the thread this message belongs to.
    #[serde(default, rename = "threadId")]
    pub thread_id: String,
}

/// A Gmail message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Message {
    /// Gmail message id.
    pub id: String,
    /// Id of the thread this message belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "threadId")]
    pub thread_id: Option<String>,
    /// Labels currently applied to this message.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "labelIds")]
    pub label_ids: Vec<String>,
    /// A short, plain-text snippet of the message body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Epoch milliseconds as a decimal string — Gmail's own wire format.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "internalDate"
    )]
    pub internal_date: Option<String>,
    /// The mailbox history id at the time this message last changed.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "historyId")]
    pub history_id: Option<String>,
    /// Estimated size of the message in bytes.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sizeEstimate"
    )]
    pub size_estimate: Option<i64>,
    /// The parsed MIME structure (headers, body parts). Preserved as raw
    /// JSON — see the module doc for why. Present when `format` is
    /// `full`, `metadata`, or `minimal` with parts; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// The full RFC 2822 message, base64url-encoded. Present only when
    /// `format=raw` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl Message {
    /// Parses [`Self::internal_date`] into a UTC timestamp.
    ///
    /// Returns `None` when absent or unparsable — Gmail's field is a
    /// decimal-string epoch-millisecond value; malformed input degrades
    /// gracefully rather than erroring.
    #[must_use]
    pub fn internal_date_utc(&self) -> Option<DateTime<Utc>> {
        let ms: i64 = self.internal_date.as_deref()?.parse().ok()?;
        DateTime::from_timestamp_millis(ms)
    }
}

/// Response envelope for `GET /gmail/v1/users/{userId}/messages`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageListResponse {
    /// Matching messages on this page.
    #[serde(default)]
    pub messages: Vec<MessageRef>,
    /// Cursor for the next page, when more results are available.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "nextPageToken"
    )]
    pub next_page_token: Option<String>,
    /// Gmail's estimate of the total number of matches.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "resultSizeEstimate"
    )]
    pub result_size_estimate: Option<i64>,
}

/// A Gmail thread.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Thread {
    /// Gmail thread id.
    pub id: String,
    /// The mailbox history id at the time this thread last changed.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "historyId")]
    pub history_id: Option<String>,
    /// A short, plain-text snippet of the thread's most relevant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Messages in the thread.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Message>,
}

/// A thread reference, as returned by `threads.list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ThreadRef {
    /// Gmail thread id.
    pub id: String,
    /// A short, plain-text snippet of the thread's most relevant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// The mailbox history id at the time this thread last changed.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "historyId")]
    pub history_id: Option<String>,
}

/// Response envelope for `GET /gmail/v1/users/{userId}/threads`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadListResponse {
    /// Matching threads on this page.
    #[serde(default)]
    pub threads: Vec<ThreadRef>,
    /// Cursor for the next page, when more results are available.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "nextPageToken"
    )]
    pub next_page_token: Option<String>,
    /// Gmail's estimate of the total number of matches.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "resultSizeEstimate"
    )]
    pub result_size_estimate: Option<i64>,
}

/// A label's display colour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LabelColor {
    /// Text colour, as a hex string (e.g. `#000000`).
    #[serde(rename = "textColor")]
    pub text_color: String,
    /// Background colour, as a hex string.
    #[serde(rename = "backgroundColor")]
    pub background_color: String,
}

/// A Gmail label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Label {
    /// Gmail label id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `"system"` (Gmail-provided, e.g. `INBOX`) or `"user"` (user-created).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    pub label_type: Option<String>,
    /// Whether messages with this label appear in the message list.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "messageListVisibility"
    )]
    pub message_list_visibility: Option<String>,
    /// Whether this label appears in the label list.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "labelListVisibility"
    )]
    pub label_list_visibility: Option<String>,
    /// Total number of messages with this label.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "messagesTotal"
    )]
    pub messages_total: Option<i64>,
    /// Number of unread messages with this label.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "messagesUnread"
    )]
    pub messages_unread: Option<i64>,
    /// Total number of threads with this label.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "threadsTotal"
    )]
    pub threads_total: Option<i64>,
    /// Number of unread threads with this label.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "threadsUnread"
    )]
    pub threads_unread: Option<i64>,
    /// Display colour, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<LabelColor>,
}

impl Label {
    /// Whether this is a Gmail-provided system label (e.g. `INBOX`,
    /// `TRASH`) rather than a user-created one.
    #[must_use]
    pub fn is_system(&self) -> bool {
        self.label_type.as_deref() == Some("system")
    }
}

/// Response envelope for `GET /gmail/v1/users/{userId}/labels`.
///
/// Unpaginated — Gmail returns every label in one call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LabelListResponse {
    /// All labels on the mailbox.
    #[serde(default)]
    pub labels: Vec<Label>,
}

/// A message added in a history record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HistoryMessageAdded {
    /// The message that was added.
    pub message: MessageRef,
}

/// A message deleted in a history record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HistoryMessageDeleted {
    /// The message that was deleted.
    pub message: MessageRef,
}

/// A label-change event in a history record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HistoryLabelChanged {
    /// The message whose labels changed.
    pub message: MessageRef,
    /// The label ids that were added or removed.
    #[serde(default, rename = "labelIds")]
    pub label_ids: Vec<String>,
}

/// One entry in the mailbox's change history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HistoryRecord {
    /// This history record's id.
    pub id: String,
    /// Messages added since the previous record.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "messagesAdded"
    )]
    pub messages_added: Vec<HistoryMessageAdded>,
    /// Messages deleted since the previous record.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "messagesDeleted"
    )]
    pub messages_deleted: Vec<HistoryMessageDeleted>,
    /// Labels added to messages since the previous record.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "labelsAdded")]
    pub labels_added: Vec<HistoryLabelChanged>,
    /// Labels removed from messages since the previous record.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "labelsRemoved"
    )]
    pub labels_removed: Vec<HistoryLabelChanged>,
}

/// Response envelope for `GET /gmail/v1/users/{userId}/history`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryListResponse {
    /// History records since the requested `startHistoryId`.
    #[serde(default)]
    pub history: Vec<HistoryRecord>,
    /// Cursor for the next page, when more results are available.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "nextPageToken"
    )]
    pub next_page_token: Option<String>,
    /// The mailbox's current `historyId`. Present only on the *last*
    /// page (when [`Self::next_page_token`] is absent). Not consumed by
    /// anything in Phase 1 — Phase 2's incremental sync will use this as
    /// its next watermark.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "historyId")]
    pub history_id: Option<String>,
}

impl JsonlSerialize for Message {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

impl JsonlSerialize for MessageListResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.messages.iter(), out)
    }
}

impl JsonlSerialize for Thread {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

impl JsonlSerialize for ThreadListResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.threads.iter(), out)
    }
}

impl JsonlSerialize for Label {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

impl JsonlSerialize for LabelListResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.labels.iter(), out)
    }
}

impl JsonlSerialize for HistoryListResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.history.iter(), out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn message_list_response_deserializes_a_realistic_fixture() {
        let json = serde_json::json!({
            "messages": [
                {"id": "msg1", "threadId": "thread1"},
                {"id": "msg2", "threadId": "thread1"},
            ],
            "nextPageToken": "page2",
            "resultSizeEstimate": 2,
        });
        let response: MessageListResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.messages.len(), 2);
        assert_eq!(response.messages[0].id, "msg1");
        assert_eq!(response.next_page_token.as_deref(), Some("page2"));
    }

    #[test]
    fn message_deserializes_nested_mime_payload_and_round_trips() {
        let json = serde_json::json!({
            "id": "msg1",
            "threadId": "thread1",
            "labelIds": ["INBOX", "UNREAD"],
            "snippet": "Hello there",
            "internalDate": "1700000000000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [{"name": "Subject", "value": "Hi"}],
                "parts": [
                    {
                        "mimeType": "multipart/alternative",
                        "parts": [
                            {"mimeType": "text/plain", "body": {"data": "aGVsbG8"}},
                            {"mimeType": "text/html", "body": {"data": "PGI+aGVsbG88L2I+"}},
                        ]
                    }
                ]
            }
        });
        let message: Message = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(message.id, "msg1");
        assert_eq!(message.label_ids, vec!["INBOX", "UNREAD"]);
        assert_eq!(message.payload, Some(json["payload"].clone()));

        // Round-trips the nested structure losslessly.
        let reserialized = serde_json::to_value(&message).unwrap();
        assert_eq!(reserialized["payload"], json["payload"]);
    }

    #[test]
    fn message_with_format_minimal_has_no_payload() {
        let json = serde_json::json!({"id": "msg1", "labelIds": ["INBOX"]});
        let message: Message = serde_json::from_value(json).unwrap();
        assert_eq!(message.payload, None);
    }

    #[test]
    fn message_internal_date_utc_parses_valid_epoch_ms() {
        let message = Message {
            internal_date: Some("1700000000000".to_string()),
            ..Default::default()
        };
        let dt = message.internal_date_utc().unwrap();
        assert_eq!(dt.timestamp_millis(), 1_700_000_000_000);
    }

    #[test]
    fn message_internal_date_utc_is_none_for_missing_or_invalid() {
        assert_eq!(Message::default().internal_date_utc(), None);
        let message = Message {
            internal_date: Some("not-a-number".to_string()),
            ..Default::default()
        };
        assert_eq!(message.internal_date_utc(), None);
    }

    #[test]
    fn thread_deserializes_embedded_messages() {
        let json = serde_json::json!({
            "id": "thread1",
            "historyId": "1000",
            "messages": [
                {"id": "msg1", "threadId": "thread1"},
                {"id": "msg2", "threadId": "thread1"},
            ]
        });
        let thread: Thread = serde_json::from_value(json).unwrap();
        assert_eq!(thread.messages.len(), 2);
        assert_eq!(thread.history_id.as_deref(), Some("1000"));
    }

    #[test]
    fn label_deserializes_with_and_without_color() {
        let with_color = serde_json::json!({
            "id": "Label_1",
            "name": "Finance",
            "type": "user",
            "color": {"textColor": "#000000", "backgroundColor": "#ffffff"},
        });
        let label: Label = serde_json::from_value(with_color).unwrap();
        assert!(label.color.is_some());
        assert!(!label.is_system());

        let without_color = serde_json::json!({"id": "INBOX", "name": "INBOX", "type": "system"});
        let label: Label = serde_json::from_value(without_color).unwrap();
        assert!(label.color.is_none());
        assert!(label.is_system());
    }

    #[test]
    fn history_record_deserializes_populated_fixture() {
        let json = serde_json::json!({
            "id": "12345",
            "messagesAdded": [{"message": {"id": "m1", "threadId": "t1"}}],
            "labelsAdded": [{"message": {"id": "m2", "threadId": "t1"}, "labelIds": ["IMPORTANT"]}],
            "labelsRemoved": [{"message": {"id": "m3", "threadId": "t1"}, "labelIds": ["UNREAD"]}],
        });
        let record: HistoryRecord = serde_json::from_value(json).unwrap();
        assert_eq!(record.messages_added.len(), 1);
        assert_eq!(record.labels_added[0].label_ids, vec!["IMPORTANT"]);
        assert_eq!(record.labels_removed[0].label_ids, vec!["UNREAD"]);
        assert!(record.messages_deleted.is_empty());
    }

    #[test]
    fn message_write_jsonl_emits_exactly_one_line() {
        let message = Message {
            id: "msg1".to_string(),
            ..Default::default()
        };
        let mut buf = Vec::new();
        message.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("msg1"));
    }

    #[test]
    fn message_list_response_write_jsonl_emits_one_line_per_message() {
        let response = MessageListResponse {
            messages: vec![
                MessageRef {
                    id: "m1".to_string(),
                    thread_id: "t1".to_string(),
                },
                MessageRef {
                    id: "m2".to_string(),
                    thread_id: "t1".to_string(),
                },
            ],
            ..Default::default()
        };
        let mut buf = Vec::new();
        response.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn label_list_response_write_jsonl_emits_one_line_per_label() {
        let response = LabelListResponse {
            labels: vec![
                Label {
                    id: "L1".to_string(),
                    name: "One".to_string(),
                    ..Default::default()
                },
                Label {
                    id: "L2".to_string(),
                    name: "Two".to_string(),
                    ..Default::default()
                },
            ],
        };
        let mut buf = Vec::new();
        response.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn deserializing_unmodeled_extra_field_still_succeeds() {
        // Forward-compatible with Google adding fields: confirms no
        // accidental `#[serde(deny_unknown_fields)]`.
        let json = serde_json::json!({
            "id": "msg1",
            "threadId": "t1",
            "driveDetails": {"somethingNew": true},
        });
        let message: Message = serde_json::from_value(json).unwrap();
        assert_eq!(message.id, "msg1");
    }

    #[test]
    fn message_list_response_empty_page_with_valid_next_page_token_deserializes() {
        // The Gmail-specific pagination quirk: a filtered list can return
        // zero results alongside a valid nextPageToken.
        let json = serde_json::json!({"messages": [], "nextPageToken": "page2"});
        let response: MessageListResponse = serde_json::from_value(json).unwrap();
        assert!(response.messages.is_empty());
        assert_eq!(response.next_page_token.as_deref(), Some("page2"));
    }
}
