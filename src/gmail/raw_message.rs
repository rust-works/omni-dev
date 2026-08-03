//! Decoding and light header extraction for `format=raw` Gmail messages.
//!
//! `format=raw` returns the complete RFC 2822 message, base64url-encoded.
//! [`decode_raw_message`] (promoted here from `src/cli/gmail/read.rs`, fixed
//! in commit `d7b6c0a0`) turns that back into real bytes; [`extract_headers`]
//! pulls a handful of headers out of those bytes without a MIME parser or a
//! second `format=metadata` network round-trip — `gmail sync`
//! (`src/cli/gmail/sync/engine.rs`) needs both to write a byte-exact `.eml`
//! and populate its manifest from data already in hand.

use anyhow::{Context, Result};
use base64::Engine as _;
use std::collections::HashMap;

use crate::gmail::types::Message;

/// Base64url-decodes [`Message::raw`] into the literal RFC 2822 bytes Gmail
/// returned — a genuine byte-exact copy, not a preview.
///
/// Gmail's own encoder omits padding, but this decodes leniently either way
/// by stripping any trailing `=` before decoding unpadded.
pub(crate) fn decode_raw_message(message: &Message) -> Result<Vec<u8>> {
    let raw = message
        .raw
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Gmail's response for `--detail raw` had no `raw` field"))?;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim_end_matches('='))
        .context("Failed to base64url-decode the raw RFC 2822 message")
}

/// Extracts the first occurrence of each requested header (case-insensitive)
/// from an RFC 5322 message's header block.
///
/// Not a MIME parser: this only scans the header block (stopping at the
/// first blank line, the RFC 5322 header/body boundary), honours
/// folded/continuation lines (a line starting with a space or tab continues
/// the previous header's value), and does not decode RFC 2047 encoded-words
/// (`=?UTF-8?B?...?=`) — non-ASCII header values are returned as their raw
/// wire encoding. Missing/malformed `names` entries are simply absent from
/// the result rather than erroring, since header presence varies by message.
pub(crate) fn extract_headers(raw: &[u8], names: &[&str]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(raw);
    let mut result = HashMap::new();

    let mut current: Option<(String, String)> = None;
    let flush = |current: Option<(String, String)>, result: &mut HashMap<String, String>| {
        if let Some((name, value)) = current {
            let matched = names.iter().find(|n| n.eq_ignore_ascii_case(&name));
            if let Some(&matched_name) = matched {
                result.entry(matched_name.to_string()).or_insert(value);
            }
        }
    };

    for line in text.split(['\n']) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            break; // end of header block
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Folded continuation of the previous header's value.
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            flush(current.take(), &mut result);
            current = Some((name.trim().to_string(), value.trim().to_string()));
        }
    }
    flush(current, &mut result);

    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── decode_raw_message ───────────────────────────────────────────

    #[test]
    fn decode_raw_message_decodes_unpadded_base64url() {
        let source = "From: a@example.com\r\nSubject: Hi\r\n\r\nBody text.";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        let message = Message {
            id: "m1".to_string(),
            raw: Some(encoded),
            ..Default::default()
        };
        let decoded = decode_raw_message(&message).unwrap();
        assert_eq!(decoded, source.as_bytes());
    }

    #[test]
    fn decode_raw_message_tolerates_padded_base64url() {
        let source = "From: a@example.com\r\n\r\nBody.";
        // `URL_SAFE` (padded) rather than `URL_SAFE_NO_PAD`: real-world
        // encoders aren't guaranteed to omit padding, so decoding must not
        // assume Gmail's own convention is the only valid input.
        let encoded = base64::engine::general_purpose::URL_SAFE.encode(source);
        let message = Message {
            id: "m1".to_string(),
            raw: Some(encoded),
            ..Default::default()
        };
        let decoded = decode_raw_message(&message).unwrap();
        assert_eq!(decoded, source.as_bytes());
    }

    #[test]
    fn decode_raw_message_errors_when_raw_field_absent() {
        let message = Message {
            id: "m1".to_string(),
            ..Default::default()
        };
        let err = decode_raw_message(&message).unwrap_err();
        assert!(err.to_string().contains("no `raw` field"));
    }

    #[test]
    fn decode_raw_message_errors_on_malformed_base64() {
        let message = Message {
            id: "m1".to_string(),
            raw: Some("not valid base64url!!!".to_string()),
            ..Default::default()
        };
        let err = decode_raw_message(&message).unwrap_err();
        assert!(err.to_string().contains("base64url-decode"));
    }

    // ── extract_headers ────────────────────────────────────────────────

    #[test]
    fn extract_headers_matches_requested_names_case_insensitively() {
        let raw = b"From: a@example.com\r\nSUBJECT: Hello\r\nMessage-Id: <1@x>\r\n\r\nBody";
        let headers = extract_headers(raw, &["From", "Subject", "Message-Id"]);
        assert_eq!(
            headers.get("From").map(String::as_str),
            Some("a@example.com")
        );
        assert_eq!(headers.get("Subject").map(String::as_str), Some("Hello"));
        assert_eq!(headers.get("Message-Id").map(String::as_str), Some("<1@x>"));
    }

    #[test]
    fn extract_headers_stops_at_first_blank_line() {
        let raw = b"From: a@example.com\r\n\r\nSubject: not-a-header-its-body";
        let headers = extract_headers(raw, &["Subject"]);
        assert!(!headers.contains_key("Subject"));
    }

    #[test]
    fn extract_headers_unfolds_continuation_lines() {
        let raw = b"Subject: Hello\r\n world\r\n\r\nBody";
        let headers = extract_headers(raw, &["Subject"]);
        assert_eq!(
            headers.get("Subject").map(String::as_str),
            Some("Hello world")
        );
    }

    #[test]
    fn extract_headers_ignores_unrequested_headers() {
        let raw = b"From: a@example.com\r\nX-Other: whatever\r\n\r\nBody";
        let headers = extract_headers(raw, &["From"]);
        assert_eq!(headers.len(), 1);
        assert!(!headers.contains_key("X-Other"));
    }

    #[test]
    fn extract_headers_keeps_first_occurrence_of_a_duplicated_header() {
        let raw = b"Received: first\r\nReceived: second\r\n\r\nBody";
        let headers = extract_headers(raw, &["Received"]);
        assert_eq!(headers.get("Received").map(String::as_str), Some("first"));
    }

    #[test]
    fn extract_headers_returns_empty_map_when_none_present() {
        let raw = b"X-Only: value\r\n\r\nBody";
        let headers = extract_headers(raw, &["From", "Subject"]);
        assert!(headers.is_empty());
    }
}
