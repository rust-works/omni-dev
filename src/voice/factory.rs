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
//!    release cycle; pick `--backend whisper-candle` explicitly. See
//!    [`crate::voice::backends::candle`] and ADR-0033.
//!
//! A fourth name, `"voxtral"`, is recognised on every host but is
//! platform-gated (#933, ADR-0037): the native engine compiles only on
//! `cfg(not(target_os = "windows"))` behind the off-by-default `voxtral`
//! feature, and requesting it where it is unavailable is a clear
//! construction-time error rather than a build break or a silent fallback.
//!
//! [`create_default_claude_client`]: crate::claude::client::create_default_claude_client

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::voice::backends::candle::CandleTranscriber;
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
        "voxtral" => create_voxtral_transcriber(opts),
        other => {
            bail!(
                "unknown voice backend: {other:?} \
                 (supported: \"mock\", \"whisper-candle\", \"voxtral\")"
            )
        }
    }
}

/// Constructs the native Voxtral backend (#933, [ADR-0037]).
///
/// The native engine (vendored `antirez/voxtral.c` behind a `voxtral-sys` FFI
/// crate) is permitted only behind a Rust FFI boundary on
/// `cfg(not(target_os = "windows"))`, and only when the off-by-default
/// `voxtral` Cargo feature is enabled. When compiled in, this resolves the
/// Voxtral model directory and constructs a
/// [`crate::voice::backends::voxtral::VoxtralBackend`]; the model-bound
/// construction errors (missing weights) carry an install hint.
///
/// The `"voxtral"` backend name is recognised on **every** host — including
/// Windows and feature-off builds — per ADR-0035's cross-platform-descriptor
/// contract: an unavailable backend yields a clear, actionable
/// *construction-time* error explaining **why** it is unavailable, never an
/// "unknown backend" error and never a build break (ADR-0037 §3). There is no
/// silent fall-through to `whisper-candle`: an explicit `--backend voxtral`
/// that cannot be served fails loudly rather than substituting a different
/// backend behind the user's back.
///
/// [ADR-0037]: ../../../docs/adrs/adr-0037.md
fn create_voxtral_transcriber(opts: &VoiceOpts) -> Result<Box<dyn Transcriber>> {
    #[cfg(all(feature = "voxtral", not(target_os = "windows")))]
    {
        use crate::voice::backends::voxtral::{VoxtralBackend, DEFAULT_VOXTRAL_DELAY_MS};
        use crate::voice::models::resolve_voxtral_model_dir;

        let dir = resolve_voxtral_model_dir(opts)?;
        Ok(Box::new(VoxtralBackend::new(
            &dir,
            DEFAULT_VOXTRAL_DELAY_MS,
        )?))
    }

    #[cfg(target_os = "windows")]
    {
        // `opts` is unused on the error-only arms; mark it used in every config.
        let _ = opts;
        // Native Voxtral is excluded on Windows by design (ADR-0037): the Metal
        // fast path is Apple-only and the project takes on no Windows
        // native-toolchain requirement. `whisper-candle` is the supported
        // cross-platform alternative.
        bail!(
            "voice backend \"voxtral\" is not available on Windows by design \
             (ADR-0037); use --backend whisper-candle"
        )
    }

    #[cfg(all(not(feature = "voxtral"), not(target_os = "windows")))]
    {
        let _ = opts;
        // Supported target, but built without the opt-in feature.
        bail!(
            "voice backend \"voxtral\" was not compiled in; rebuild with \
             `--features voxtral` on macOS or Linux (the native engine requires \
             a C toolchain — see ADR-0037), or use --backend whisper-candle"
        )
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
        assert!(msg.contains("voxtral"), "got: {msg}");
    }

    // The "voxtral" backend name is recognised on every host (ADR-0035's
    // cross-platform-descriptor contract), so it never surfaces as an "unknown
    // backend". What it resolves to is platform- and feature-dependent
    // (ADR-0037); each build configuration gets its own clear construction-time
    // error rather than a build break or a silent fallback to whisper-candle.

    /// Helper: route `--backend voxtral` through the factory and return the
    /// error it must produce on every host where the native engine is absent.
    #[cfg(not(all(feature = "voxtral", not(target_os = "windows"))))]
    fn voxtral_error() -> String {
        let _g = env_guard();
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
        let opts = VoiceOpts {
            backend: Some("voxtral".to_string()),
            model: None,
        };
        // `.err().expect()` rather than `let Err(..) else { panic!() }`: the
        // call always errors here, so the `else` arm would be a permanently
        // uncovered branch. The test module allows `expect_used`.
        create_default_transcriber(&opts)
            .err()
            .expect("expected voxtral to error where the native engine is absent")
            .to_string()
    }

    #[cfg(all(feature = "voxtral", not(target_os = "windows")))]
    #[test]
    fn voxtral_feature_on_propagates_missing_model_error() {
        // With the feature compiled in, the factory routes "voxtral" through
        // VoxtralBackend::new, which calls ensure_voxtral_model_present. Point
        // --model at an empty dir and verify the install hint reaches the
        // caller (mirroring whisper_candle_arm_propagates_missing_model_error).
        let _g = env_guard();
        std::env::remove_var("OMNI_DEV_VOICE_BACKEND");
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = VoiceOpts {
            backend: Some("voxtral".to_string()),
            model: Some(tmp.path().to_path_buf()),
        };
        let err = create_default_transcriber(&opts)
            .err()
            .expect("expected voxtral with empty model dir to error");
        let msg = format!("{err:#}");
        assert!(msg.contains("no Voxtral model found"), "got: {msg}");
        assert!(
            msg.contains("--variant voxtral-mini-4b-realtime"),
            "got: {msg}"
        );
        assert!(!msg.contains("unknown"), "got: {msg}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn voxtral_on_windows_reports_unavailable_by_design() {
        let msg = voxtral_error();
        assert!(msg.contains("voxtral"), "got: {msg}");
        assert!(msg.contains("Windows"), "got: {msg}");
        assert!(msg.contains("whisper-candle"), "got: {msg}");
        assert!(!msg.contains("unknown"), "got: {msg}");
    }

    #[cfg(all(not(feature = "voxtral"), not(target_os = "windows")))]
    #[test]
    fn voxtral_feature_off_reports_not_compiled_in() {
        let msg = voxtral_error();
        assert!(msg.contains("voxtral"), "got: {msg}");
        assert!(msg.contains("--features voxtral"), "got: {msg}");
        assert!(!msg.contains("unknown"), "got: {msg}");
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
}
