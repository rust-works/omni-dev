//! Filesystem path sanitisation helpers.

use std::path::Path;

/// Returns a safe, single-component filename for an attachment.
///
/// Attachment titles are remote-controlled, so strip everything but the final
/// path component to prevent a malicious title (e.g. `../../etc/passwd`) from
/// escaping the destination directory. Falls back to the attachment ID when
/// the title has no usable file name.
pub(crate) fn attachment_filename(title: &str, id: &str) -> String {
    Path::new(title)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .unwrap_or_else(|| format!("attachment-{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_filename_keeps_plain_name() {
        assert_eq!(attachment_filename("diagram.png", "att-1"), "diagram.png");
    }

    #[test]
    fn attachment_filename_strips_path_traversal() {
        assert_eq!(attachment_filename("../../etc/passwd", "att-1"), "passwd");
    }

    #[test]
    fn attachment_filename_strips_absolute_path() {
        assert_eq!(attachment_filename("/etc/passwd", "att-1"), "passwd");
    }

    #[test]
    fn attachment_filename_falls_back_to_id() {
        assert_eq!(attachment_filename("", "att-1"), "attachment-att-1");
        assert_eq!(attachment_filename("..", "att-9"), "attachment-att-9");
    }
}
