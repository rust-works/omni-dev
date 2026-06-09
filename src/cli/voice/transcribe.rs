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
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::voice::models::SPEAKER_WESPEAKER_EN;
use crate::voice::{
    cosine, create_default_transcriber, detect_format, render_jsonl, render_markdown, speaker_file,
    EnrolledSpeaker, OutputFormat, TranscriptEvent, VecAudioInput, VoiceOpts, WespeakerEmbedder,
    MIN_EMBED_SAMPLES,
};

/// Default chunk size handed to [`VecAudioInput`]. Doesn't affect the
/// mock backend's output; chosen for parity with the streaming pipeline
/// (#806) where ~64 ms chunks at 16 kHz keep latency low without
/// thrashing the inference loop.
const DEFAULT_CHUNK_SAMPLES: usize = 1024;

/// Default cosine-similarity threshold for `--speaker` filtering.
///
/// Calibrated against
/// [tests/fixtures/voice/two_speakers.wav](../../../tests/fixtures/voice/two_speakers.wav)
/// in [SPIKE.md on `issue-805-spike-tract-speaker`]: within-speaker mean
/// ≈ 0.91, cross-speaker mean ≈ 0.07. The 0.5 default sits ~0.4 above
/// the cross-speaker max and ~0.4 below the within-speaker min, leaving
/// comfortable margin on both sides.
pub const DEFAULT_SPEAKER_THRESHOLD: f32 = 0.5;

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

    /// Transcriber backend (`mock`, `whisper-candle`, `voxtral`, `voxtral-mlx`).
    /// Defaults to `mock`; see ADR-0033 for the `whisper-candle` runtime,
    /// ADR-0037 for the platform-gated native `voxtral` backend (opt-in,
    /// macOS/Linux), and ADR-0039 for the real-time INT4 `voxtral-mlx` backend
    /// (opt-in, macOS Apple Silicon only).
    #[arg(long)]
    pub backend: Option<String>,

    /// Path to a backend-specific model directory. For `whisper-candle`,
    /// this overrides `OMNI_DEV_VOICE_WHISPER_MODEL` and the default at
    /// `~/.omni-dev/voice/models/whisper-tiny.en/`. Ignored by `mock`.
    #[arg(long)]
    pub model: Option<PathBuf>,

    /// Voxtral decoder delay (lookahead) in milliseconds; the #930 spike's
    /// sweet spot is 240–480 ms. Used by `--backend voxtral` and `voxtral-mlx`;
    /// ignored by `mock` and `whisper-candle`. Defaults to 480 ms.
    #[arg(long)]
    pub delay_ms: Option<i32>,

    /// Output format. Defaults to `md` on a tty, `jsonl` when piped.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatArg>,

    /// Enrolled speaker to filter on. Drops any `Final` event whose
    /// segment doesn't match the enrolled embedding by cosine
    /// similarity at or above `--threshold`.
    #[arg(long)]
    pub speaker: Option<String>,

    /// Cosine-similarity threshold for `--speaker`. Defaults to 0.5;
    /// see [`DEFAULT_SPEAKER_THRESHOLD`].
    #[arg(long)]
    pub threshold: Option<f32>,

    /// Path to the wespeaker ONNX model. Overrides the default at
    /// `~/.omni-dev/voice/models/wespeaker-en-voxceleb-resnet34-LM/` and
    /// `OMNI_DEV_VOICE_SPEAKER_MODEL`. Ignored unless `--speaker` is
    /// set.
    #[arg(long)]
    pub speaker_model: Option<PathBuf>,
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
    /// Thin shim around `Self::run`: locks stdout and resolves the
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
        let speaker_filter = self
            .speaker
            .as_deref()
            .map(|name| {
                SpeakerFilter::load(
                    name,
                    self.speaker_model.as_deref(),
                    self.threshold.unwrap_or(DEFAULT_SPEAKER_THRESHOLD),
                    &self.wav,
                )
            })
            .transpose()?;
        let opts = VoiceOpts {
            backend: self.backend,
            model: self.model,
            delay_ms: self.delay_ms,
        };
        let transcriber = create_default_transcriber(&opts)?;
        let input = VecAudioInput::from_wav_path(&self.wav, DEFAULT_CHUNK_SAMPLES)?;
        let stream = transcriber.transcribe(Box::new(input))?;

        // Collect the (small, batch) stream so we can fold the speaker
        // filter over it without juggling lifetime gymnastics on the
        // boxed event iterator.
        let events: Vec<Result<TranscriptEvent>> = stream.collect();
        let filtered: Vec<Result<TranscriptEvent>> = match &speaker_filter {
            Some(f) => events
                .into_iter()
                .filter_map(|ev| f.transform(ev))
                .collect(),
            None => events,
        };

        match format {
            OutputFormat::Jsonl => render_jsonl(filtered, w)?,
            OutputFormat::Md => render_markdown(filtered, w)?,
        }
        w.flush()?;
        Ok(())
    }
}

/// Wraps the enrolled-speaker embedding + embedder + source PCM needed
/// to filter the `Final` event stream on a single speaker.
struct SpeakerFilter {
    name: String,
    enrolled: EnrolledSpeaker,
    embedder: WespeakerEmbedder,
    pcm: Vec<i16>,
    threshold: f32,
}

impl SpeakerFilter {
    fn load(name: &str, speaker_model: Option<&Path>, threshold: f32, wav: &Path) -> Result<Self> {
        let enrolled_path = speaker_file(name)?;
        let enrolled = EnrolledSpeaker::load(&enrolled_path).with_context(|| {
            format!(
                "load enrolled speaker {} from {}",
                name,
                enrolled_path.display()
            )
        })?;
        let dir = SPEAKER_WESPEAKER_EN.resolve_dir(speaker_model)?;
        SPEAKER_WESPEAKER_EN.ensure_present(&dir)?;
        let model_path = dir.join(SPEAKER_WESPEAKER_EN.required_files[0]);
        let embedder = WespeakerEmbedder::new(&model_path)?;
        let pcm = read_wav_pcm_16k_mono(wav)?;
        Ok(Self {
            name: name.to_string(),
            enrolled,
            embedder,
            pcm,
            threshold,
        })
    }

    /// Filters a single event. Returns `Some(event)` to keep it (with
    /// `speaker` set on `Final`) or `None` to drop it. `Partial` and
    /// `Endpoint` events always pass through unchanged. Errors pass
    /// through so downstream rendering can fail loudly.
    fn transform(&self, ev: Result<TranscriptEvent>) -> Option<Result<TranscriptEvent>> {
        let ev = match ev {
            Ok(ev) => ev,
            err @ Err(_) => return Some(err),
        };
        match ev {
            TranscriptEvent::Final {
                event_id,
                text,
                start,
                end,
                confidence,
                words,
                speaker: _,
                revisable,
            } => {
                let s = (start.as_secs_f64() * 16_000.0) as usize;
                let e = (end.as_secs_f64() * 16_000.0) as usize;
                let lo = s.min(self.pcm.len());
                let hi = e.min(self.pcm.len());
                let window = &self.pcm[lo..hi.max(lo)];
                if window.len() < MIN_EMBED_SAMPLES {
                    // Too short for a stable embedding; conservatively drop.
                    return None;
                }
                let emb = match self.embedder.embed(window) {
                    Ok(v) => v,
                    Err(err) => return Some(Err(err)),
                };
                if cosine(&emb, &self.enrolled.vector) >= self.threshold {
                    Some(Ok(TranscriptEvent::Final {
                        event_id,
                        text,
                        start,
                        end,
                        confidence,
                        words,
                        speaker: Some(self.name.clone()),
                        revisable,
                    }))
                } else {
                    None
                }
            }
            other => Some(Ok(other)),
        }
    }
}

/// Reads a 16 kHz mono 16-bit signed PCM WAV from `path`, returning the
/// raw samples for re-windowing by [`SpeakerFilter::transform`].
///
/// Delegates format validation to the same invariants
/// [`VecAudioInput::from_wav_path`] enforces; the two paths read the
/// file independently because the transcriber moves its input.
fn read_wav_pcm_16k_mono(path: &Path) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("open WAV at {} for speaker filter", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != 16_000
        || spec.channels != 1
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        bail!(
            "WAV at {} must be 16 kHz mono 16-bit PCM for --speaker filtering",
            path.display()
        );
    }
    reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("decode PCM samples from {}", path.display()))
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
    fn parses_delay_ms_flag() {
        let cli = TestCli::try_parse_from(["test", "/tmp/x.wav", "--delay-ms", "300"]).unwrap();
        assert_eq!(cli.transcribe.delay_ms, Some(300));
        // Absent by default.
        let bare = TestCli::try_parse_from(["test", "/tmp/x.wav"]).unwrap();
        assert!(bare.transcribe.delay_ms.is_none());
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
    fn parses_speaker_flag() {
        let cli = TestCli::try_parse_from(["test", "/tmp/x.wav", "--speaker", "alice"]).unwrap();
        assert_eq!(cli.transcribe.speaker.as_deref(), Some("alice"));
        // Threshold defaults to None at parse time; the run path applies
        // DEFAULT_SPEAKER_THRESHOLD when speaker is set and threshold is None.
        assert!(cli.transcribe.threshold.is_none());
    }

    #[test]
    fn parses_threshold_flag() {
        let cli = TestCli::try_parse_from(["test", "/tmp/x.wav", "--threshold", "0.65"]).unwrap();
        assert!((cli.transcribe.threshold.unwrap() - 0.65).abs() < f32::EPSILON);
    }

    #[test]
    fn parses_speaker_model_flag() {
        let cli = TestCli::try_parse_from([
            "test",
            "/tmp/x.wav",
            "--speaker-model",
            "/opt/wespeaker.onnx",
        ])
        .unwrap();
        assert_eq!(
            cli.transcribe
                .speaker_model
                .as_deref()
                .and_then(|p| p.to_str()),
            Some("/opt/wespeaker.onnx")
        );
    }

    #[test]
    fn rejects_non_numeric_threshold() {
        let result = TestCli::try_parse_from(["test", "/tmp/x.wav", "--threshold", "high"]);
        assert!(result.is_err(), "non-numeric threshold should fail");
    }

    #[test]
    fn default_speaker_threshold_is_half() {
        assert!(
            (DEFAULT_SPEAKER_THRESHOLD - 0.5).abs() < f32::EPSILON,
            "default threshold must be 0.5 to match the spike-calibrated default"
        );
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
            delay_ms: None,
            format: None,
            speaker: None,
            threshold: None,
            speaker_model: None,
        }
    }

    fn write_test_wav(
        path: &std::path::Path,
        sample_rate: u32,
        channels: u16,
        bits: u16,
        format: hound::SampleFormat,
    ) {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: bits,
            sample_format: format,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        match format {
            hound::SampleFormat::Int => {
                for s in [0_i16, 1, 2, 3] {
                    writer.write_sample(s).unwrap();
                }
            }
            hound::SampleFormat::Float => {
                writer.write_sample(0.0_f32).unwrap();
            }
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn read_wav_pcm_16k_mono_accepts_valid_wav() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("ok.wav");
        write_test_wav(&path, 16_000, 1, 16, hound::SampleFormat::Int);
        let pcm = read_wav_pcm_16k_mono(&path).unwrap();
        assert_eq!(pcm, vec![0, 1, 2, 3]);
    }

    #[test]
    fn read_wav_pcm_16k_mono_rejects_wrong_sample_rate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("44k.wav");
        write_test_wav(&path, 44_100, 1, 16, hound::SampleFormat::Int);
        let err = read_wav_pcm_16k_mono(&path).unwrap_err();
        assert!(
            err.to_string().contains("must be 16 kHz mono 16-bit PCM"),
            "got: {err}"
        );
    }

    #[test]
    fn read_wav_pcm_16k_mono_rejects_stereo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("stereo.wav");
        write_test_wav(&path, 16_000, 2, 16, hound::SampleFormat::Int);
        let err = read_wav_pcm_16k_mono(&path).unwrap_err();
        assert!(err.to_string().contains("16 kHz mono"), "got: {err}");
    }

    #[test]
    fn read_wav_pcm_16k_mono_rejects_wrong_bit_depth() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("24bit.wav");
        write_test_wav(&path, 16_000, 1, 24, hound::SampleFormat::Int);
        let err = read_wav_pcm_16k_mono(&path).unwrap_err();
        assert!(err.to_string().contains("16 kHz mono"), "got: {err}");
    }

    #[test]
    fn read_wav_pcm_16k_mono_rejects_float_format() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("f32.wav");
        write_test_wav(&path, 16_000, 1, 32, hound::SampleFormat::Float);
        let err = read_wav_pcm_16k_mono(&path).unwrap_err();
        assert!(err.to_string().contains("16 kHz mono"), "got: {err}");
    }

    #[test]
    fn read_wav_pcm_16k_mono_missing_file_errors() {
        let err = read_wav_pcm_16k_mono(std::path::Path::new("/nope/missing.wav")).unwrap_err();
        assert!(err.to_string().contains("open WAV"), "got: {err}");
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
