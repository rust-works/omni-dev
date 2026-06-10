//! Backend factory for [`crate::voice::Transcriber`].
//!
//! Mirrors the [`create_default_claude_client`] dispatch pattern (one
//! short-circuit per supported backend, sensible default last). Backend
//! choice flows from, in order:
//!
//! 1. `opts.backend` (set by `--backend` from the CLI in #802),
//! 2. `OMNI_DEV_VOICE_BACKEND` (env var, with project settings.json
//!    fallback via [`crate::utils::settings::get_env_var`]),
//! 3. Default — `"mock"` until the real ASR backend has been through a
//!    release cycle; pick `--backend whisper-candle` (batch) or
//!    `--backend whisper-candle-streaming` (latency-tolerant streaming)
//!    explicitly. See [`crate::voice::backends::candle`],
//!    [`crate::voice::backends::candle_streaming`], ADR-0033, and
//!    ADR-0040.
//!
//! [`create_default_claude_client`]: crate::claude::client::create_default_claude_client

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::voice::backends::candle::CandleTranscriber;
use crate::voice::backends::candle_streaming::CandleStreamingTranscriber;
use crate::voice::backends::mock::MockTranscriber;
use crate::voice::models::resolve_whisper_model_dir;
use crate::voice::transcriber::Transcriber;

/// Backend-selection options carried from the CLI (or constructed
/// programmatically for tests).
///
/// `model` is plumbed through for future backends; `MockTranscriber`
/// ignores it. When the real ASR backend lands this field will resolve
/// the model file path (see #801 spec — `--model` → env
/// `OMNI_DEV_VOICE_WHISPER_MODEL` → `~/.omni-dev/voice/models/...`).
#[derive(Debug, Default, Clone)]
pub struct VoiceOpts {
    /// Explicit backend choice from `--backend`. `None` means "fall back
    /// to env var, then default."
    pub backend: Option<String>,
    /// Path to a backend-specific model file. Ignored by the mock.
    pub model: Option<PathBuf>,
}

/// Constructs the appropriate [`Transcriber`] given `opts` and the
/// process environment.
///
/// Errors only on an unrecognised backend name. Backend-specific
/// construction errors (missing model file, failed initialisation) bubble
/// up from the backend's own `new`.
pub fn create_default_transcriber(opts: &VoiceOpts) -> Result<Box<dyn Transcriber>> {
    let backend = opts
        .backend
        .clone()
        .or_else(|| crate::utils::settings::get_env_var("OMNI_DEV_VOICE_BACKEND").ok())
        .unwrap_or_else(|| "mock".to_string());

    match backend.as_str() {
        "mock" => Ok(Box::new(MockTranscriber::new(
            MockTranscriber::default_script(),
        ))),
        "whisper-candle" => {
            let dir = resolve_whisper_model_dir(opts)?;
            Ok(Box::new(CandleTranscriber::new(&dir)?))
        }
        "whisper-candle-streaming" => {
            let dir = resolve_whisper_model_dir(opts)?;
            Ok(Box::new(CandleStreamingTranscriber::new(&dir)?))
        }
        other => {
            bail!(
                "unknown voice backend: {other:?} (supported: \"mock\", \"whisper-candle\", \"whisper-candle-streaming\")"
            )
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::transcriber::{TranscriptEvent, VecAudioInput};
    use std::sync::{Mutex, MutexGuard};

    // Guard so env-var-mutating tests in this module don't race each other.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        match ENV_GUARD.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn collect(transcriber: &dyn Transcriber) -> Vec<TranscriptEvent> {
        let input = VecAudioInput::from_samples(vec![0; 16_000], 1024);
        transcriber
            .transcribe(Box::new(input))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn default_backend_is_mock() {
        let _g = env_guard();
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
        let t = create_default_transcriber(&VoiceOpts::default()).unwrap();
        let events = collect(t.as_ref());
        // Default script has 2 Finals; mock always appends 1 Endpoint.
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], TranscriptEvent::Final { .. }));
        assert!(matches!(events[1], TranscriptEvent::Final { .. }));
        assert!(matches!(events[2], TranscriptEvent::Endpoint { .. }));
    }

    #[test]
    fn opts_backend_takes_precedence_over_env() {
        let _g = env_guard();
        std::env::set_var("OMNI_DEV_VOICE_BACKEND", "this-would-fail-if-read");
        let opts = VoiceOpts {
            backend: Some("mock".to_string()),
            model: None,
        };
        let t = create_default_transcriber(&opts).unwrap();
        let _ = collect(t.as_ref());
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
    }

    #[test]
    fn env_var_selects_backend_when_opts_absent() {
        let _g = env_guard();
        std::env::set_var("OMNI_DEV_VOICE_BACKEND", "mock");
        let t = create_default_transcriber(&VoiceOpts::default()).unwrap();
        let _ = collect(t.as_ref());
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
    }

    #[test]
    fn unknown_backend_errors() {
        let _g = env_guard();
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
        let opts = VoiceOpts {
            backend: Some("klingon".to_string()),
            model: None,
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected unknown backend to error");
        };
        let msg = err.to_string();
        assert!(msg.contains("klingon"), "got: {msg}");
        assert!(msg.contains("supported"), "got: {msg}");
        assert!(msg.contains("whisper-candle"), "got: {msg}");
        assert!(msg.contains("whisper-candle-streaming"), "got: {msg}");
    }

    #[test]
    fn whisper_candle_arm_propagates_missing_model_error() {
        // The factory routes "whisper-candle" through CandleTranscriber::new,
        // which calls ensure_model_present. Point --model at an empty dir
        // and verify the install hint reaches the caller without partial
        // initialisation.
        let _g = env_guard();
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = VoiceOpts {
            backend: Some("whisper-candle".to_string()),
            model: Some(tmp.path().to_path_buf()),
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected whisper-candle with empty model dir to error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Whisper model found"), "got: {msg}");
        assert!(msg.contains("voice install-model"), "got: {msg}");
    }

    #[test]
    fn whisper_candle_streaming_arm_propagates_missing_model_error() {
        // Same install-hint contract as the batch arm: the streaming
        // backend loads the same model files via WhisperEngine::load.
        let _g = env_guard();
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = VoiceOpts {
            backend: Some("whisper-candle-streaming".to_string()),
            model: Some(tmp.path().to_path_buf()),
        };
        let Err(err) = create_default_transcriber(&opts) else {
            panic!("expected whisper-candle-streaming with empty model dir to error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Whisper model found"), "got: {msg}");
        assert!(msg.contains("voice install-model"), "got: {msg}");
    }
}
