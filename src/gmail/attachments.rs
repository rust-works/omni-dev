//! Real MIME/multipart attachment extraction for `gmail sync
//! --extract-attachments`.
//!
//! Deliberately separate from [`crate::gmail::raw_message`]'s
//! `extract_attachment_filenames` heuristic scanner: writing attachment
//! *bytes* to disk needs real multipart boundary parsing and
//! Content-Transfer-Encoding decoding, which a line-oriented header scan
//! can't provide. This is the only module in the crate that pulls in a full
//! MIME parser (`mail-parser`), and it's only ever reached when
//! `--extract-attachments` is set — the manifest's `attachment_count`/
//! `attachment_filenames` fields keep coming from the cheap heuristic
//! regardless, by design (see ADR-0065).

use std::collections::HashSet;

use mail_parser::{ContentType, MessageParser, MimeHeaders};

use crate::utils::path::attachment_filename;

/// One attachment extracted from a raw MIME message, ready to write to
/// disk: `filename` is already sanitised (traversal-safe, via
/// [`attachment_filename`]) and de-duplicated against its siblings in the
/// same message.
pub(crate) struct ExtractedAttachment {
    pub(crate) filename: String,
    pub(crate) contents: Vec<u8>,
}

/// Parses `raw` as a MIME message and returns every `Content-Disposition:
/// attachment` part's sanitised, de-duplicated filename and decoded
/// contents.
///
/// Mirrors `extract_attachment_filenames`'s rule of ignoring `inline`
/// dispositions, but as a real parser it additionally resolves RFC 2231
/// continuation parameters (`filename*0=`/`filename*1*=`, ...) and fully
/// decodes Content-Transfer-Encoding (base64, quoted-printable, ...) —
/// neither of which the heuristic scanner attempts. An unparseable message
/// (no headers found at all) yields an empty `Vec` rather than an error:
/// extraction is a convenience projection over an already-written `.eml`,
/// never a reason to fail the fetch.
pub(crate) fn extract_attachments(raw: &[u8]) -> Vec<ExtractedAttachment> {
    let Some(message) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    message
        .attachments()
        .filter(|part| {
            part.content_disposition()
                .is_some_and(ContentType::is_attachment)
        })
        .enumerate()
        .map(|(index, part)| {
            let name =
                attachment_filename(part.attachment_name().unwrap_or(""), &index.to_string());
            ExtractedAttachment {
                filename: dedupe_filename(&mut seen, name),
                contents: part.contents().to_vec(),
            }
        })
        .collect()
}

/// Appends a `-N` suffix before the extension the first time `name` repeats
/// within one call's `seen` set (e.g. `image.png` -> `image-1.png` ->
/// `image-2.png`), so two same-named attachments in one message don't
/// overwrite each other on disk. Collisions are detected case-*insensitively*
/// (`seen` is keyed by the lowercased name, though the returned filename
/// keeps its original casing) because the destination filesystem might be
/// too: macOS (APFS) and Windows (NTFS) both default to case-insensitive,
/// so `Report.PDF` and `report.pdf` name the same directory entry there and
/// a case-sensitive check would let the second silently overwrite the
/// first.
fn dedupe_filename(seen: &mut HashSet<String>, name: String) -> String {
    if seen.insert(name.to_ascii_lowercase()) {
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), Some(ext.to_string())),
        _ => (name.clone(), None),
    };
    let mut n = 1u32;
    loop {
        let candidate = match &ext {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{stem}-{n}"),
        };
        if seen.insert(candidate.to_ascii_lowercase()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn extract_attachments_decodes_base64_binary_content() {
        let bytes: &[u8] = &[0x50, 0x44, 0x46, 0x00, 0x01, 0x02, 0xff];
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nHello\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\r\n{encoded}\r\n\
--B--\r\n"
        );
        let extracted = extract_attachments(raw.as_bytes());
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].filename, "report.pdf");
        assert_eq!(extracted[0].contents, bytes);
    }

    #[test]
    fn extract_attachments_decodes_quoted_printable_content() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nHello\r\n\
--B\r\nContent-Type: text/plain; charset=\"utf-8\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\
Content-Disposition: attachment; filename=\"notes.txt\"\r\n\r\nCaf=C3=A9\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].filename, "notes.txt");
        assert_eq!(extracted[0].contents, "Café".as_bytes());
    }

    #[test]
    fn extract_attachments_decodes_rfc2231_percent_encoded_filename() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename*=UTF-8''report%20Q3.pdf\r\n\r\ndata\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].filename, "report Q3.pdf");
    }

    #[test]
    fn extract_attachments_resolves_rfc2231_continuation_filename() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename*0=\"report_\"; filename*1=\"Q3.pdf\"\r\n\r\ndata\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].filename, "report_Q3.pdf");
    }

    #[test]
    fn extract_attachments_synthesizes_name_for_unnamed_attachment() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment\r\n\r\ndata\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].filename, "attachment-0");
    }

    #[test]
    fn extract_attachments_excludes_inline_disposition() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: image/png\r\nContent-Disposition: inline; filename=\"logo.png\"\r\n\r\ndata\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert!(extracted.is_empty());
    }

    #[test]
    fn extract_attachments_dedupes_same_named_attachments_within_one_message() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: image/png\r\nContent-Disposition: attachment; filename=\"image.png\"\r\n\r\nfirst\r\n\
--B\r\nContent-Type: image/png\r\nContent-Disposition: attachment; filename=\"image.png\"\r\n\r\nsecond\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0].filename, "image.png");
        assert_eq!(extracted[1].filename, "image-1.png");
    }

    #[test]
    fn extract_attachments_dedupes_names_differing_only_by_case() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"Report.PDF\"\r\n\r\nfirst\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\n\r\nsecond\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0].filename, "Report.PDF");
        assert_eq!(extracted[1].filename, "report-1.pdf");
    }

    #[test]
    fn extract_attachments_sanitizes_path_traversal_filename() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"../../etc/passwd\"\r\n\r\ndata\r\n\
--B--\r\n";
        let extracted = extract_attachments(raw);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].filename, "passwd");
    }

    #[test]
    fn extract_attachments_returns_empty_vec_for_unparseable_message() {
        assert!(extract_attachments(b"").is_empty());
    }

    #[test]
    fn extract_attachments_returns_empty_vec_for_plain_text_message() {
        let raw = b"Subject: Hi\r\nFrom: a@example.com\r\n\r\nJust text, no attachments.";
        assert!(extract_attachments(raw).is_empty());
    }
}
