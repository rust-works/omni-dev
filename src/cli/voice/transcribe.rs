//! `omni-dev voice transcribe` — feed a 16 kHz mono WAV file through the
//! configured [`crate::voice::Transcriber`] and emit JSONL events to stdout
//! (markdown when stdout is a tty).
//!
//! WAV validation is delegated to
//! [`crate::voice::VecAudioInput::from_wav_path`] — non-16 kHz, non-mono,
//! non-16-bit-PCM files error with a descriptive message pointing at
//! `voice capture` as the source of normalised audio.

use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::voice::{
    create_default_transcriber, detect_format, render_jsonl, render_markdown, OutputFormat,
    VecAudioInput, VoiceOpts,
};

/// Default chunk size handed to [`VecAudioInput`]. Doesn't affect the
/// mock backend's output; chosen for parity with the streaming pipeline
/// (#806) where ~64 ms chunks at 16 kHz keep latency low without
/// thrashing the inference loop.
const DEFAULT_CHUNK_SAMPLES: usize = 1024;

/// Transcribes a 16 kHz mono WAV file to JSONL or markdown.
///
/// Output format defaults to `md` on a tty and `jsonl` when stdout is
/// piped; pass `--format` to override. The transcriber backend is chosen
/// by `--backend`, then `OMNI_DEV_VOICE_BACKEND`, then the default
/// (`"mock"` until a real ASR backend lands — see ADR-0032).
#[derive(Parser)]
pub struct TranscribeCommand {
    /// Path to a 16 kHz mono 16-bit PCM WAV file. Use `voice capture` to
    /// produce one — `transcribe` does not resample.
    pub wav: PathBuf,

    /// Transcriber backend (`mock`, `whisper-candle`). Defaults to `mock`;
    /// see ADR-0033 for the `whisper-candle` runtime choice.
    #[arg(long)]
    pub backend: Option<String>,

    /// Path to a backend-specific model directory. For `whisper-candle`,
    /// this overrides `OMNI_DEV_VOICE_WHISPER_MODEL` and the default at
    /// `~/.omni-dev/voice/models/whisper-tiny.en/`. Ignored by `mock`.
    #[arg(long)]
    pub model: Option<PathBuf>,

    /// Output format. Defaults to `md` on a tty, `jsonl` when piped.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatArg>,
}

/// `clap` value enum matching [`OutputFormat`].
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum OutputFormatArg {
    /// JSON Lines — one event per line, machine-readable.
    Jsonl,
    /// Markdown — human-readable transcript view.
    Md,
}

impl From<OutputFormatArg> for OutputFormat {
    fn from(value: OutputFormatArg) -> Self {
        match value {
            OutputFormatArg::Jsonl => Self::Jsonl,
            OutputFormatArg::Md => Self::Md,
        }
    }
}

impl TranscribeCommand {
    /// Executes the transcribe command.
    ///
    /// Thin shim around [`Self::run`]: locks stdout and resolves the
    /// effective format from `--format` plus tty auto-detection, then
    /// delegates to the writer-generic helper. The split keeps stdout-
    /// locking and tty-detection out of the testable business logic.
    pub fn execute(self) -> Result<()> {
        let format = detect_format(
            self.format.map(OutputFormat::from),
            std::io::stdout().is_terminal(),
        );
        let mut out = std::io::stdout().lock();
        self.run(&mut out, format)
    }

    /// Runs the transcribe pipeline against an arbitrary writer.
    ///
    /// Decoupled from stdout so unit tests can drive the error paths
    /// (writer failures, flush failures, backend-construction failures)
    /// without spawning a subprocess.
    fn run<W: Write>(self, w: &mut W, format: OutputFormat) -> Result<()> {
        let opts = VoiceOpts {
            backend: self.backend,
            model: self.model,
        };
        let transcriber = create_default_transcriber(&opts)?;
        let input = VecAudioInput::from_wav_path(&self.wav, DEFAULT_CHUNK_SAMPLES)?;
        let stream = transcriber.transcribe(Box::new(input))?;

        match format {
            OutputFormat::Jsonl => render_jsonl(stream, w)?,
            OutputFormat::Md => render_markdown(stream, w)?,
        }
        w.flush()?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        transcribe: TranscribeCommand,
    }

    #[test]
    fn parses_required_wav_only() {
        let cli = TestCli::try_parse_from(["test", "/tmp/x.wav"]).unwrap();
        assert_eq!(cli.transcribe.wav.to_str().unwrap(), "/tmp/x.wav");
        assert!(cli.transcribe.backend.is_none());
        assert!(cli.transcribe.model.is_none());
        assert!(cli.transcribe.format.is_none());
    }

    #[test]
    fn parses_model_flag() {
        let cli =
            TestCli::try_parse_from(["test", "/tmp/x.wav", "--model", "/opt/whisper"]).unwrap();
        assert_eq!(
            cli.transcribe.model.as_deref().and_then(|p| p.to_str()),
            Some("/opt/whisper")
        );
    }

    #[test]
    fn parses_all_flags() {
        let cli = TestCli::try_parse_from([
            "test",
            "/tmp/x.wav",
            "--backend",
            "mock",
            "--format",
            "jsonl",
        ])
        .unwrap();
        assert_eq!(cli.transcribe.backend.as_deref(), Some("mock"));
        assert!(matches!(
            cli.transcribe.format,
            Some(OutputFormatArg::Jsonl)
        ));
    }

    #[test]
    fn parses_md_format() {
        let cli = TestCli::try_parse_from(["test", "/tmp/x.wav", "--format", "md"]).unwrap();
        assert!(matches!(cli.transcribe.format, Some(OutputFormatArg::Md)));
    }

    #[test]
    fn rejects_missing_wav() {
        let result = TestCli::try_parse_from(["test"]);
        assert!(result.is_err(), "wav argument is required");
    }

    #[test]
    fn rejects_unknown_format() {
        let result = TestCli::try_parse_from(["test", "/tmp/x.wav", "--format", "yaml"]);
        assert!(result.is_err(), "only md/jsonl are valid formats");
    }

    #[test]
    fn output_format_arg_maps_to_output_format() {
        assert_eq!(
            OutputFormat::from(OutputFormatArg::Jsonl),
            OutputFormat::Jsonl
        );
        assert_eq!(OutputFormat::from(OutputFormatArg::Md), OutputFormat::Md);
    }

    // ── In-process error-path tests for `TranscribeCommand::run` ──
    //
    // These exercise the `?` propagation in `run` directly, bypassing the
    // subprocess machinery so each error site (backend factory, WAV load,
    // render write, render flush) is hit deterministically.

    struct AlwaysFailWriter;

    impl Write for AlwaysFailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("forced write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FlushFailWriter;

    impl Write for FlushFailWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("forced flush failure"))
        }
    }

    fn fixture_wav() -> PathBuf {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest.join("tests/fixtures/voice/short_en.wav")
    }

    fn cmd(wav: PathBuf, backend: Option<&str>) -> TranscribeCommand {
        TranscribeCommand {
            wav,
            backend: backend.map(str::to_string),
            model: None,
            format: None,
        }
    }

    #[test]
    fn run_propagates_unknown_backend_error() {
        let mut buf: Vec<u8> = Vec::new();
        let err = cmd(fixture_wav(), Some("nope"))
            .run(&mut buf, OutputFormat::Jsonl)
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown voice backend"),
            "got: {err}"
        );
    }

    #[test]
    fn run_propagates_missing_wav_error() {
        let mut buf: Vec<u8> = Vec::new();
        let err = cmd(PathBuf::from("/nonexistent/should/not/exist.wav"), None)
            .run(&mut buf, OutputFormat::Jsonl)
            .unwrap_err();
        assert!(err.to_string().contains("Failed to open WAV"), "got: {err}");
    }

    #[test]
    fn run_propagates_writer_error_jsonl() {
        let err = cmd(fixture_wav(), None)
            .run(&mut AlwaysFailWriter, OutputFormat::Jsonl)
            .unwrap_err();
        assert!(
            err.to_string().contains("forced write failure"),
            "got: {err}"
        );
    }

    #[test]
    fn run_propagates_writer_error_md() {
        let err = cmd(fixture_wav(), None)
            .run(&mut AlwaysFailWriter, OutputFormat::Md)
            .unwrap_err();
        assert!(
            err.to_string().contains("forced write failure"),
            "got: {err}"
        );
    }

    #[test]
    fn run_propagates_flush_error() {
        // FlushFailWriter accepts writes — including the per-event flushes
        // inside render_jsonl — so this primarily exercises the first
        // mid-stream flush `?`. The final outer `w.flush()?` in `run` is
        // only reachable if the inner flushes all succeed, which by design
        // of FlushFailWriter they don't. The behaviour we're locking in
        // is "flush errors propagate, full stop" — not which `?` site
        // catches them first.
        let err = cmd(fixture_wav(), None)
            .run(&mut FlushFailWriter, OutputFormat::Jsonl)
            .unwrap_err();
        assert!(
            err.to_string().contains("forced flush failure"),
            "got: {err}"
        );
    }
}
