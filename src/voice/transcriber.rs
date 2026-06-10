//! Transcriber trait and event types per issues #799 and #801.
//!
//! `Transcriber` is the contract every speech-to-text backend implements,
//! whether batch (this issue) or streaming (#806). It takes an
//! [`AudioInput`] producing 16 kHz mono signed-PCM chunks and returns an
//! [`EventStream`] of [`TranscriptEvent`]s.
//!
//! The separation from [`crate::voice::AudioSource`] (#800) is deliberate:
//! `AudioSource` is the *hardware-capture* seam (variable rate, variable
//! channels, `f32`, intentionally `!Send` on macOS per ADR-0031);
//! `AudioInput` is the *post-mixdown-and-resample* seam (16 kHz mono i16,
//! `Send`) that ASR engines consume natively. See ADR-0032 for the rationale.

use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

/// Monotonically-unique identifier for a `Final` event, used by downstream
/// consumers (commit-message generation, history merging) to deduplicate
/// across overlapping streaming windows.
///
/// ULID rather than UUIDv4 because we want timestamp ordering when finals
/// arrive out-of-order from a streaming backend (#806). Per #799.
pub type EventId = ulid::Ulid;

/// Diarisation tag attached to a segment when speaker labelling is on
/// (#805). Always `None` for the batch backend in #801.
pub type SpeakerId = String;

/// 16 kHz mono signed 16-bit PCM samples, in capture order.
///
/// Chunk size is up to the [`AudioInput`] implementation; a `Transcriber`
/// drains every chunk before running inference. Empty chunks are permitted
/// and treated as "more is coming".
pub type AudioChunk = Vec<i16>;

/// Source of 16 kHz mono signed-PCM audio for transcription.
///
/// Distinct from [`crate::voice::AudioSource`] (which is `!Send`, f32, and
/// variable-rate) — see the module docs and ADR-0032 for why the seam
/// splits here.
pub trait AudioInput: Send {
    /// Returns the next chunk of samples, or `None` when the input is
    /// exhausted. Implementations may yield chunks of any size; consumers
    /// must not rely on a particular chunk boundary.
    fn next_chunk(&mut self) -> Option<AudioChunk>;
}

/// Stream of transcription events. A blanket impl is provided for any
/// iterator producing `Result<TranscriptEvent>` that is also `Send`.
///
/// Sync `Iterator` shape for the batch backend in #801; the async `Stream`
/// variant lands alongside streaming work in #806.
pub trait EventStream: Iterator<Item = Result<TranscriptEvent>> + Send {}

impl<T> EventStream for T where T: Iterator<Item = Result<TranscriptEvent>> + Send {}

/// First-class word-level alignment, optionally returned by backends that
/// expose it. The batch backend in #801 always emits `None`; word-level
/// alignment is a backend opt-in, not a guarantee.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// The word's text, as it appeared in the source language.
    pub text: String,
    /// Start of the word, in stream-relative seconds.
    #[serde(with = "duration_secs")]
    pub start: Duration,
    /// End of the word, in stream-relative seconds.
    #[serde(with = "duration_secs")]
    pub end: Duration,
    /// Per-word confidence in `[0.0, 1.0]`, when the backend provides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// What ended a speech region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// A silence gap exceeded the endpointer's threshold.
    SilenceGap,
    /// The speaker explicitly stopped (e.g. push-to-talk release).
    UtteranceEnd,
    /// The input source signalled end-of-stream.
    StreamEnd,
}

/// One event emitted by a [`Transcriber`].
///
/// `Partial` carries no `event_id` because partials supersede each other —
/// only `Final` is durable enough to deduplicate against. Per #799.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// Hypothesis text that may still change. Streaming backends emit
    /// these; the batch backend in #801 never does.
    Partial {
        /// The current best-guess text for this region.
        text: String,
        /// Start of the region, in stream-relative seconds.
        #[serde(with = "duration_secs")]
        start: Duration,
        /// End of the region, in stream-relative seconds.
        #[serde(with = "duration_secs")]
        end: Duration,
        /// Word-level alignment, when the backend provides it.
        #[serde(skip_serializing_if = "Option::is_none")]
        words: Option<Vec<Word>>,
        /// Diarisation tag, when speaker labelling is on (#805).
        #[serde(skip_serializing_if = "Option::is_none")]
        speaker: Option<SpeakerId>,
    },
    /// Committed text for a region. `revisable` is `false` for batch
    /// backends and for streaming backends that have endpointed the
    /// region; `true` only when a streaming backend may still revise
    /// the text in a later pass.
    Final {
        /// Unique identifier for deduplication across overlapping windows.
        event_id: EventId,
        /// The committed transcript text.
        text: String,
        /// Start of the region, in stream-relative seconds.
        #[serde(with = "duration_secs")]
        start: Duration,
        /// End of the region, in stream-relative seconds.
        #[serde(with = "duration_secs")]
        end: Duration,
        /// Segment-level confidence in `[0.0, 1.0]`.
        confidence: f32,
        /// Word-level alignment, when the backend provides it.
        #[serde(skip_serializing_if = "Option::is_none")]
        words: Option<Vec<Word>>,
        /// Diarisation tag, when speaker labelling is on (#805).
        #[serde(skip_serializing_if = "Option::is_none")]
        speaker: Option<SpeakerId>,
        /// Whether this Final may still be revised by a later pass. Batch
        /// backends always set this to `false`.
        revisable: bool,
    },
    /// Marks the end of a speech region or the stream itself.
    Endpoint {
        /// Time of the endpoint, in stream-relative seconds.
        #[serde(with = "duration_secs")]
        at: Duration,
        /// What kind of endpoint this is.
        kind: EndpointKind,
    },
}

/// Speech-to-text backend.
///
/// `Send + Sync` so a single transcriber can be shared across worker
/// threads (e.g., one model, many concurrent inputs). Backends that hold
/// non-thread-safe handles internally wrap them in `Mutex`.
pub trait Transcriber: Send + Sync {
    /// Consumes an audio input and returns the resulting event stream.
    fn transcribe(&self, audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>>;
}

/// Async stream of transcription events produced by a [`StreamingTranscriber`].
///
/// The streaming analogue of [`EventStream`]: a `Send` `futures::Stream` the
/// caller pulls at its own pace as live audio is consumed. See
/// [ADR-0038](../../docs/adrs/adr-0038.md).
pub type TranscriptEventStream = Pin<Box<dyn Stream<Item = Result<TranscriptEvent>> + Send>>;

/// Async source of 16 kHz mono signed-PCM audio for *streaming* transcription.
///
/// The async analogue of [`AudioInput`] (same `AudioChunk` payload). A live
/// source (`voice listen`, #807) `await`s the next captured chunk; a
/// [`crate::voice::stream_input::FileAsyncAudioInput`] replays a fixture, optionally
/// on a simulated-realtime clock. `None` signals end-of-stream.
#[async_trait]
pub trait AsyncAudioInput: Send {
    /// Returns the next chunk of samples, or `None` when the input is
    /// exhausted. Chunk size is up to the implementation (the streaming
    /// convention is 100 ms / 1600 samples at 16 kHz).
    async fn next_chunk(&mut self) -> Option<AudioChunk>;
}

/// Streaming speech-to-text backend.
///
/// The streaming counterpart of [`Transcriber`]: it consumes an
/// [`AsyncAudioInput`] incrementally and returns a [`TranscriptEventStream`]
/// that emits `Partial` hypotheses, revisable `Final`s, and `Endpoint`s as the
/// audio arrives — rather than draining the whole input first. A backend may
/// implement [`Transcriber`] (batch), `StreamingTranscriber` (streaming), or
/// both. See [ADR-0038](../../docs/adrs/adr-0038.md); real streaming backends
/// land in #806 / #933 Phase 6.
pub trait StreamingTranscriber: Send + Sync {
    /// Consumes an async audio input and returns the resulting event stream.
    fn transcribe_stream(&self, audio: Box<dyn AsyncAudioInput>) -> TranscriptEventStream;
}

/// In-memory [`AudioInput`] adapter — reads a 16 kHz mono 16-bit PCM WAV
/// from disk (or accepts an in-memory `Vec<i16>`) and yields it in fixed-
/// size chunks.
///
/// Refuses WAVs that are not 16 kHz mono 16-bit signed PCM: the contract
/// of [`AudioInput`] is that samples are already at the rate the
/// transcriber expects. Resampling, channel mixdown, and bit-depth
/// conversion happen *before* a `VecAudioInput` is constructed (in the
/// streaming pipeline, that's downstream of [`crate::voice::AudioSource`]).
#[derive(Debug)]
pub struct VecAudioInput {
    samples: Vec<i16>,
    cursor: usize,
    chunk_samples: usize,
}

impl VecAudioInput {
    /// Loads a 16 kHz mono i16 PCM WAV from `path` and chunks it into
    /// pieces of `chunk_samples` samples each (last chunk may be shorter).
    /// `chunk_samples` is clamped to at least 1.
    pub fn from_wav_path(path: impl AsRef<Path>, chunk_samples: usize) -> Result<Self> {
        let path = path.as_ref();
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("Failed to open WAV at {}", path.display()))?;
        let spec = reader.spec();
        if spec.sample_rate != 16_000 {
            bail!(
                "WAV at {} must be 16000 Hz (got {}). Resample before constructing VecAudioInput.",
                path.display(),
                spec.sample_rate
            );
        }
        if spec.channels != 1 {
            bail!(
                "WAV at {} must be mono (got {} channels). Mix down before constructing VecAudioInput.",
                path.display(),
                spec.channels
            );
        }
        if spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
            bail!(
                "WAV at {} must be 16-bit signed PCM (got {}-bit {:?})",
                path.display(),
                spec.bits_per_sample,
                spec.sample_format
            );
        }
        let samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("Failed to decode i16 PCM samples from {}", path.display()))?;
        Ok(Self::from_samples(samples, chunk_samples))
    }

    /// Builds an input from an in-memory `Vec<i16>` (already 16 kHz mono).
    /// Useful for synthesised test signals.
    pub fn from_samples(samples: Vec<i16>, chunk_samples: usize) -> Self {
        Self {
            samples,
            cursor: 0,
            chunk_samples: chunk_samples.max(1),
        }
    }
}

impl AudioInput for VecAudioInput {
    fn next_chunk(&mut self) -> Option<AudioChunk> {
        if self.cursor >= self.samples.len() {
            return None;
        }
        let end = (self.cursor + self.chunk_samples).min(self.samples.len());
        let chunk = self.samples[self.cursor..end].to_vec();
        self.cursor = end;
        Some(chunk)
    }
}

/// Serde helper: serialises a `Duration` as a floating-point number of
/// seconds, so JSONL snapshots are human-readable and diff-friendly.
mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(d.as_secs_f64())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = f64::deserialize(d)?;
        Ok(Duration::from_secs_f64(secs.max(0.0)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture_wav(
        dir: &TempDir,
        name: &str,
        sample_rate: u32,
        channels: u16,
        bits: u16,
        samples: &[i16],
    ) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for s in samples {
            writer.write_sample(*s).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn vec_audio_input_from_samples_chunks_correctly() {
        let mut input = VecAudioInput::from_samples(vec![1, 2, 3, 4, 5], 2);
        assert_eq!(input.next_chunk(), Some(vec![1, 2]));
        assert_eq!(input.next_chunk(), Some(vec![3, 4]));
        assert_eq!(input.next_chunk(), Some(vec![5]));
        assert_eq!(input.next_chunk(), None);
    }

    #[test]
    fn vec_audio_input_zero_chunk_size_clamps_to_one() {
        let mut input = VecAudioInput::from_samples(vec![10, 20], 0);
        assert_eq!(input.next_chunk(), Some(vec![10]));
        assert_eq!(input.next_chunk(), Some(vec![20]));
        assert_eq!(input.next_chunk(), None);
    }

    #[test]
    fn vec_audio_input_empty_yields_none() {
        let mut input = VecAudioInput::from_samples(vec![], 16);
        assert!(input.next_chunk().is_none());
    }

    #[test]
    fn vec_audio_input_reads_16k_mono_i16_wav() {
        let tmp = TempDir::new().unwrap();
        let path = write_fixture_wav(&tmp, "ok.wav", 16_000, 1, 16, &[100, 200, 300, 400]);
        let mut input = VecAudioInput::from_wav_path(&path, 2).unwrap();
        assert_eq!(input.next_chunk(), Some(vec![100, 200]));
        assert_eq!(input.next_chunk(), Some(vec![300, 400]));
        assert!(input.next_chunk().is_none());
    }

    #[test]
    fn vec_audio_input_rejects_wrong_sample_rate() {
        let tmp = TempDir::new().unwrap();
        let path = write_fixture_wav(&tmp, "44k.wav", 44_100, 1, 16, &[0, 0]);
        let err = VecAudioInput::from_wav_path(&path, 16).unwrap_err();
        assert!(err.to_string().contains("16000 Hz"), "got: {err}");
    }

    #[test]
    fn vec_audio_input_rejects_stereo() {
        let tmp = TempDir::new().unwrap();
        let path = write_fixture_wav(&tmp, "stereo.wav", 16_000, 2, 16, &[0, 0, 0, 0]);
        let err = VecAudioInput::from_wav_path(&path, 16).unwrap_err();
        assert!(err.to_string().contains("mono"), "got: {err}");
    }

    #[test]
    fn vec_audio_input_rejects_wrong_bit_depth() {
        let tmp = TempDir::new().unwrap();
        let path = dir_with_wav_f32(&tmp);
        let err = VecAudioInput::from_wav_path(&path, 16).unwrap_err();
        assert!(err.to_string().contains("16-bit"), "got: {err}");
    }

    fn dir_with_wav_f32(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("f32.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0.0_f32).unwrap();
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn vec_audio_input_missing_file_errors() {
        let err = VecAudioInput::from_wav_path("/nope/does/not/exist.wav", 16).unwrap_err();
        assert!(err.to_string().contains("Failed to open WAV"), "got: {err}");
    }

    #[test]
    fn event_stream_blanket_impl_compiles() {
        // Just ensure `Vec<Result<TranscriptEvent>>::into_iter()` satisfies
        // `EventStream` so backends can build their streams trivially.
        fn accepts(_s: Box<dyn EventStream>) {}
        let events: Vec<Result<TranscriptEvent>> = vec![Ok(TranscriptEvent::Endpoint {
            at: Duration::from_secs(1),
            kind: EndpointKind::StreamEnd,
        })];
        accepts(Box::new(events.into_iter()));
    }

    #[test]
    fn transcript_event_serde_round_trips() {
        let event = TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, 1),
            text: "hello".to_string(),
            start: Duration::from_millis(0),
            end: Duration::from_millis(500),
            confidence: 0.97,
            words: None,
            speaker: None,
            revisable: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: TranscriptEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn duration_serialises_as_seconds() {
        let event = TranscriptEvent::Endpoint {
            at: Duration::from_millis(1500),
            kind: EndpointKind::StreamEnd,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"at\":1.5"),
            "duration should serialise as f64 seconds, got: {json}"
        );
    }

    #[test]
    fn duration_deserialise_rejects_non_numeric_seconds() {
        // The `duration_secs` helper's deserialize path returns an error
        // when the JSON value isn't a number — pin that behaviour so
        // future changes to the serde shape don't silently swallow it.
        let bad_json = r#"{"type":"endpoint","at":"not a number","kind":"stream_end"}"#;
        let result: Result<TranscriptEvent, _> = serde_json::from_str(bad_json);
        assert!(result.is_err(), "expected deserialization to fail");
    }

    #[test]
    fn vec_audio_input_propagates_decode_failure() {
        // Truncate a valid WAV mid-sample so hound's i16 iterator errors
        // on the last read. Exercises the `.with_context(…)` arm in
        // VecAudioInput::from_wav_path that wraps decode failures.
        let tmp = TempDir::new().unwrap();
        let path = write_fixture_wav(&tmp, "truncated.wav", 16_000, 1, 16, &[1, 2, 3, 4]);
        let len = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 1)
            .unwrap();
        let err = VecAudioInput::from_wav_path(&path, 16).unwrap_err();
        assert!(
            err.to_string().contains("Failed to decode i16 PCM samples"),
            "got: {err}"
        );
    }
}
