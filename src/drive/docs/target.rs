//! Classifying a Drive file id for the Docs commands, and wording the
//! refusal when it is not a Google Doc.
//!
//! Shared by the read verbs and (later) the gated write verbs, which need
//! the identical classification plus the target's `parents` for the
//! permission gate — so the mime-type reasoning lives in one place rather
//! than being copy-pasted into each engine.
//!
//! These are *nonsense* refusals, not policy ones: `drive docs read` on a
//! spreadsheet is not disallowed, it is meaningless. That is the same
//! distinction `content_edit.rs`'s Google-native refusal draws and
//! `sheets/write.rs`'s inverts, and it is why they are checked before any
//! gate rather than through one.

use anyhow::Result;

use crate::drive::files_api::FilesApi;
use crate::drive::types::{
    DriveFile, GOOGLE_DOC_MIME_TYPE, GOOGLE_SHEET_MIME_TYPE, GOOGLE_SHORTCUT_MIME_TYPE,
    GOOGLE_SLIDES_MIME_TYPE,
};

/// What a Drive file id turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocTarget {
    /// A Google Doc. Carries the full metadata, so a write engine can read
    /// `parents` for the gate without a second `files.get`.
    Document(Box<DriveFile>),
    /// A shortcut. Never followed — its own variant, because "this is not a
    /// Google Doc" is a confusing thing to say about a shortcut *to* one.
    Shortcut {
        /// The shortcut's name, for the message.
        name: String,
    },
    /// Anything else.
    NotADocument {
        /// The file's name, for the message.
        name: String,
        /// Its mime type, which is what makes a targeted hint possible.
        mime_type: String,
    },
}

impl DocTarget {
    /// Classifies already-fetched metadata.
    ///
    /// Split from [`classify`] so the mime-type reasoning is testable
    /// without a server, and so a write engine that already holds the
    /// metadata does not fetch it twice.
    #[must_use]
    pub fn of(file: DriveFile) -> Self {
        // Shortcut first: a shortcut *to* a Doc would otherwise fall through
        // to the generic arm and be described as "not a Google Doc", which
        // is both true and useless.
        if file.mime_type == GOOGLE_SHORTCUT_MIME_TYPE {
            return Self::Shortcut { name: file.name };
        }
        if file.mime_type == GOOGLE_DOC_MIME_TYPE {
            return Self::Document(Box::new(file));
        }
        Self::NotADocument {
            name: file.name,
            mime_type: file.mime_type,
        }
    }

    /// The document's metadata, when this is one.
    #[must_use]
    pub fn document(&self) -> Option<&DriveFile> {
        match self {
            Self::Document(file) => Some(file),
            _ => None,
        }
    }

    /// The refusal message, or `None` when this *is* a Google Doc.
    ///
    /// `verb` is the CLI spelling (`read`, `info`, …) so the message names
    /// the command the user actually typed.
    #[must_use]
    pub fn refusal_message(&self, verb: &str) -> Option<String> {
        match self {
            Self::Document(_) => None,
            Self::Shortcut { name } => Some(format!(
                "Refused: '{name}' is a shortcut; `drive docs {verb}` doesn't follow shortcuts — \
                 resolve the target document's id and use that instead"
            )),
            Self::NotADocument { name, mime_type } => {
                let base = format!(
                    "Refused: '{name}' is not a Google Doc (mimeType: {mime_type}); \
                     `drive docs {verb}` only works on Google Docs"
                );
                // The mime type is known, so pointing at the command that
                // *does* work costs one match arm and saves a support
                // round-trip.
                match mime_type.as_str() {
                    GOOGLE_SHEET_MIME_TYPE => {
                        Some(format!("{base} — try `omni-dev drive sheets read` instead"))
                    }
                    GOOGLE_SLIDES_MIME_TYPE => Some(format!(
                        "{base} — Google Slides has no `drive slides` surface yet"
                    )),
                    _ => Some(base),
                }
            }
        }
    }
}

/// Fetches a file's metadata and classifies it.
pub async fn classify(api: &FilesApi<'_>, file_id: &str) -> Result<DocTarget> {
    Ok(DocTarget::of(api.get_metadata(file_id).await?))
}

/// Replaces `err` with a Docs-specific refusal when `files.get` explains it.
///
/// The read verbs classify **lazily**: they call `documents.get` first and
/// only reach for `files.get` once it has already failed. An eager preflight
/// would double the request count on every *successful* read to improve a
/// message that only appears on an unsuccessful one — which is exactly why
/// `sheets read` does no preflight and only `sheets write` does, and there
/// only because the gate needs `parents` anyway.
///
/// Any failure to classify leaves the original error untouched: a better
/// message is a nicety, and losing the real error to chase one would be a
/// bad trade.
pub async fn explain_failure(
    api: &FilesApi<'_>,
    file_id: &str,
    verb: &str,
    err: anyhow::Error,
) -> anyhow::Error {
    match classify(api, file_id).await {
        Ok(target) => target
            .refusal_message(verb)
            .map_or(err, |message| anyhow::anyhow!(message)),
        Err(_) => err,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn file(name: &str, mime_type: &str) -> DriveFile {
        DriveFile {
            id: "d1".to_string(),
            name: name.to_string(),
            mime_type: mime_type.to_string(),
            ..DriveFile::default()
        }
    }

    #[test]
    fn classify_recognises_a_google_doc() {
        let target = DocTarget::of(file("Design Doc", GOOGLE_DOC_MIME_TYPE));
        assert!(target.refusal_message("read").is_none());
        assert_eq!(target.document().unwrap().name, "Design Doc");
    }

    /// A shortcut *to* a Doc would otherwise be described as "not a Google
    /// Doc", which is true and useless.
    #[test]
    fn classify_checks_shortcut_before_the_mime_check() {
        let target = DocTarget::of(file("Design Doc (link)", GOOGLE_SHORTCUT_MIME_TYPE));
        assert!(matches!(target, DocTarget::Shortcut { .. }));
        let message = target.refusal_message("read").unwrap();
        assert!(message.contains("is a shortcut"), "{message}");
        assert!(message.contains("doesn't follow shortcuts"), "{message}");
        assert!(!message.contains("not a Google Doc"), "{message}");
    }

    #[test]
    fn refusal_for_a_spreadsheet_points_at_drive_sheets_read() {
        let message = DocTarget::of(file("Q3", GOOGLE_SHEET_MIME_TYPE))
            .refusal_message("read")
            .unwrap();
        assert!(message.contains("not a Google Doc"), "{message}");
        assert!(message.contains("drive sheets read"), "{message}");
    }

    #[test]
    fn refusal_for_a_presentation_says_slides_is_unsupported() {
        let message = DocTarget::of(file("Deck", GOOGLE_SLIDES_MIME_TYPE))
            .refusal_message("read")
            .unwrap();
        assert!(message.contains("Google Slides"), "{message}");
    }

    #[test]
    fn refusal_for_a_plain_file_names_its_mime_type_without_a_hint() {
        let message = DocTarget::of(file("notes.txt", "text/plain"))
            .refusal_message("read")
            .unwrap();
        assert!(message.contains("text/plain"), "{message}");
        assert!(!message.contains("try `omni-dev"), "{message}");
    }

    /// The message names the command the user actually typed.
    #[test]
    fn refusal_names_the_verb() {
        let target = DocTarget::of(file("Q3", GOOGLE_SHEET_MIME_TYPE));
        assert!(target
            .refusal_message("info")
            .unwrap()
            .contains("drive docs info"));
        assert!(target
            .refusal_message("read")
            .unwrap()
            .contains("drive docs read"));
    }
}
