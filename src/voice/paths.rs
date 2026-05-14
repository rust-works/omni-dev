//! User-state directory helpers for the voice subsystem.
//!
//! Centralises the `~/.omni-dev/voice/...` layout so the capture,
//! transcribe, install-model, and enroll commands all derive paths
//! from a single source of truth.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

/// `~/.omni-dev/voice/` — root for voice-related user state.
pub fn omni_dev_voice_root() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow!("could not determine the user's home directory"))?;
    Ok(home.join(".omni-dev").join("voice"))
}

/// `~/.omni-dev/voice/captures/` — destination for `voice capture` and
/// the enrolment tempfile.
pub fn captures_dir() -> Result<PathBuf> {
    Ok(omni_dev_voice_root()?.join("captures"))
}

/// `~/.omni-dev/voice/speakers/` — destination for enrolled embeddings.
pub fn speakers_dir() -> Result<PathBuf> {
    Ok(omni_dev_voice_root()?.join("speakers"))
}

/// `~/.omni-dev/voice/speakers/<name>.json` — path for a single enrolled
/// speaker's embedding JSON.
///
/// The caller is responsible for ensuring `name` is a filename-safe
/// identifier; we reject names containing path separators or null bytes
/// here as a defence-in-depth guard.
pub fn speaker_file(name: &str) -> Result<PathBuf> {
    if name.is_empty() {
        return Err(anyhow!("speaker name must not be empty"));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(anyhow!(
            "speaker name {name:?} contains a path separator or null byte"
        ));
    }
    if name == "." || name == ".." {
        return Err(anyhow!("speaker name {name:?} is not a valid filename"));
    }
    Ok(speakers_dir()?.join(format!("{name}.json")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn omni_dev_voice_root_ends_with_voice() {
        let p = omni_dev_voice_root().unwrap();
        assert!(p.ends_with(".omni-dev/voice"));
    }

    #[test]
    fn captures_dir_is_under_voice_root() {
        let root = omni_dev_voice_root().unwrap();
        let captures = captures_dir().unwrap();
        assert_eq!(captures, root.join("captures"));
    }

    #[test]
    fn speakers_dir_is_under_voice_root() {
        let root = omni_dev_voice_root().unwrap();
        let speakers = speakers_dir().unwrap();
        assert_eq!(speakers, root.join("speakers"));
    }

    #[test]
    fn speaker_file_joins_name_dot_json() {
        let p = speaker_file("jky").unwrap();
        assert!(p.ends_with(".omni-dev/voice/speakers/jky.json"));
    }

    #[test]
    fn speaker_file_rejects_empty_name() {
        let err = speaker_file("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn speaker_file_rejects_forward_slash() {
        let err = speaker_file("a/b").unwrap_err();
        assert!(err.to_string().contains("path separator"), "got: {err}");
    }

    #[test]
    fn speaker_file_rejects_backslash() {
        let err = speaker_file("a\\b").unwrap_err();
        assert!(err.to_string().contains("path separator"), "got: {err}");
    }

    #[test]
    fn speaker_file_rejects_null_byte() {
        let err = speaker_file("a\0b").unwrap_err();
        assert!(err.to_string().contains("null byte"), "got: {err}");
    }

    #[test]
    fn speaker_file_rejects_dot_and_dotdot() {
        for &s in &[".", ".."] {
            let err = speaker_file(s).unwrap_err();
            assert!(
                err.to_string().contains("not a valid filename"),
                "got: {err}"
            );
        }
    }
}
