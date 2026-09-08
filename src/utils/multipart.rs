//! `multipart/related` (RFC 2387) body assembly for Google's simple upload
//! endpoints.
//!
//! Both Drive's `files.create?uploadType=multipart` and Gmail's
//! `messages.insert?uploadType=multipart` require exactly this format — two
//! parts, a JSON metadata part followed by the raw content part — and reject
//! the `multipart/form-data` `reqwest::multipart::Form` would produce, so
//! this can't just call into `reqwest`'s own multipart support.
//!
//! Shared rather than written once per caller because the encoding is
//! byte-exact (`\r\n` placement, no trailing bytes after the closing
//! boundary) and a single stray byte silently corrupts the uploaded object
//! rather than erroring — for `gmail insert` that means a truncated message
//! stored in someone's mailbox.

use base64::Engine as _;
use rand::Rng as _;

/// A fresh, random `multipart/related` boundary — unlikely to collide with
/// arbitrary binary content, unlike a fixed string would risk.
pub(crate) fn generate_boundary() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!(
        "omnidev-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// Hand-assembles a `multipart/related` body: a JSON metadata part, then
/// `content` spliced in verbatim under `content_type`.
///
/// `content_type` is written straight into a header line, bypassing the CRLF
/// rejection `reqwest`'s own `header()` applies — a caller passing an
/// attacker-influenced value must reject CR/LF first (see
/// `crate::drive::files_api`'s `check_content_type`); callers passing a
/// compile-time literal have nothing to check.
pub(crate) fn build_related_body(
    metadata: &serde_json::Value,
    content: &[u8],
    content_type: &str,
    boundary: &str,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(content.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(metadata.to_string().as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(content);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--").as_bytes());
    body
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── build_related_body ──────────────────────────────────────────

    #[test]
    fn multipart_body_has_two_parts_separated_by_the_boundary() {
        let metadata = serde_json::json!({"name": "photo.jpg", "parents": ["p1"]});
        let body = build_related_body(&metadata, b"JPEGDATA", "image/jpeg", "BOUNDARY");
        let body_str = String::from_utf8(body).unwrap();
        assert_eq!(
            body_str,
            "--BOUNDARY\r\n\
             Content-Type: application/json; charset=UTF-8\r\n\r\n\
             {\"name\":\"photo.jpg\",\"parents\":[\"p1\"]}\r\n\
             --BOUNDARY\r\n\
             Content-Type: image/jpeg\r\n\r\n\
             JPEGDATA\r\n\
             --BOUNDARY--"
        );
    }

    #[test]
    fn multipart_body_preserves_binary_content_byte_for_byte() {
        let metadata = serde_json::json!({"name": "bin"});
        let binary_content: Vec<u8> = vec![0x00, 0xFF, 0x0D, 0x0A, 0x2D, 0x2D, 0x01];
        let body = build_related_body(&metadata, &binary_content, "application/octet-stream", "B");
        // The exact byte sequence must appear intact, unmangled by any
        // text-mode transformation.
        let needle_pos = body
            .windows(binary_content.len())
            .position(|w| w == binary_content.as_slice());
        assert!(
            needle_pos.is_some(),
            "binary content not found intact in body"
        );
    }

    #[test]
    fn multipart_body_ends_with_the_closing_boundary_no_trailing_bytes() {
        let metadata = serde_json::json!({});
        let body = build_related_body(&metadata, b"x", "text/plain", "B");
        assert!(body.ends_with(b"--B--"));
    }

    /// A bare `\r` and a bare `\n` inside the content must survive: an
    /// archived `.eml` is arbitrary bytes, and any line-ending normalisation
    /// here would change the message Gmail stores.
    #[test]
    fn multipart_body_preserves_lone_cr_and_lf_and_boundary_like_lines() {
        let metadata = serde_json::json!({});
        let content = b"a\rb\nc\r\n--not-really-a-boundary\r\nd".to_vec();
        let body = build_related_body(&metadata, &content, "message/rfc822", "B");
        assert!(body.windows(content.len()).any(|w| w == content.as_slice()));
    }

    // ── generate_boundary ───────────────────────────────────────────

    #[test]
    fn generate_boundary_produces_distinct_values() {
        let a = generate_boundary();
        let b = generate_boundary();
        assert_ne!(a, b);
        assert!(a.starts_with("omnidev-"));
    }
}
