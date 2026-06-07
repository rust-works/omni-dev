//! Audio adapters for the streaming seam ([ADR-0038](../../docs/adrs/adr-0038.md)).
//!
//! - [`MixdownResampleAudioInput`] bridges the capture seam
//!   ([`crate::voice::AudioSource`]: f32, variable rate/channels, `!Send` on
//!   macOS) to the i16/16 kHz consumer, reusing [`mono_mixdown`] and
//!   [`Resampler`]. It implements the sync
//!   [`AudioInput`] (so it feeds either seam) and is `Send` when `S: Send`. The
//!   live cpal `!Send` source is bridged via a thread+channel in `voice listen`
//!   (#807); this adapter is used with `Send` sources here.
//! - [`FileAsyncAudioInput`] implements [`AsyncAudioInput`], replaying a 16 kHz
//!   mono i16 WAV as 100 ms chunks with an optional simulated-realtime clock so
//!   streaming backends and `voice listen` can be driven "as live" from a
//!   fixture with no microphone.

use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::voice::audio::AudioSource;
use crate::voice::transcriber::{AsyncAudioInput, AudioChunk, AudioInput, VecAudioInput};
use crate::voice::wav::{mono_mixdown, Resampler};

/// Streaming chunk size: 100 ms at 16 kHz (#806 convention).
pub const STREAM_CHUNK_SAMPLES: usize = 1600;

/// 16 kHz output sample rate (matches [`Resampler`]'s fixed target).
const TARGET_SAMPLE_RATE: f64 = 16_000.0;

/// Converts a single f32 sample in `[-1.0, 1.0]` to 16-bit signed PCM,
/// clamping any resampler overshoot (same cast the WAV writer uses).
fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

/// Adapts an [`AudioSource`] (capture-side f32, arbitrary rate/channels) into
/// the [`AudioInput`] the ASR backends consume (16 kHz mono i16).
///
/// Each [`AudioInput::next_chunk`] returns up to [`STREAM_CHUNK_SAMPLES`]
/// samples; the final chunk may be shorter. Construction takes ownership of the
/// source and builds a [`Resampler`] for its rate.
pub struct MixdownResampleAudioInput<S: AudioSource> {
    source: S,
    channels: u16,
    resampler: Resampler,
    /// 16 kHz mono i16 produced but not yet handed out.
    buffered: VecDeque<i16>,
    /// The source returned `None`; the resampler tail has been flushed.
    drained: bool,
}

impl<S: AudioSource> MixdownResampleAudioInput<S> {
    /// Builds an adapter over `source`, sizing the resampler to its rate.
    pub fn new(source: S) -> Result<Self> {
        let channels = source.channels();
        let resampler = Resampler::new(source.sample_rate())?;
        Ok(Self {
            source,
            channels,
            resampler,
            buffered: VecDeque::new(),
            drained: false,
        })
    }

    /// Pulls one source chunk, mixes to mono, resamples to 16 kHz, and appends
    /// the i16 result to `buffered`. Returns `false` once the source is
    /// exhausted (after flushing the resampler tail exactly once).
    fn pump(&mut self) -> Result<bool> {
        if let Some(chunk) = self.source.next_chunk() {
            let mono = mono_mixdown(&chunk, self.channels);
            let resampled = self.resampler.push(&mono)?;
            self.buffered
                .extend(resampled.iter().copied().map(f32_to_i16));
            Ok(true)
        } else {
            if !self.drained {
                let tail = self.resampler.flush()?;
                self.buffered.extend(tail.iter().copied().map(f32_to_i16));
                self.drained = true;
            }
            Ok(false)
        }
    }

    /// Drains the buffer into a chunk of up to [`STREAM_CHUNK_SAMPLES`].
    fn take_chunk(&mut self) -> AudioChunk {
        let n = self.buffered.len().min(STREAM_CHUNK_SAMPLES);
        self.buffered.drain(..n).collect()
    }
}

// `AudioInput: Send`, so the adapter is an `AudioInput` only for `Send` sources
// (`FileAudioSource`, synthetic). The live cpal `!Send` source is bridged via a
// thread+channel in `voice listen` (#807).
impl<S: AudioSource + Send> AudioInput for MixdownResampleAudioInput<S> {
    fn next_chunk(&mut self) -> Option<AudioChunk> {
        // Accumulate at least one full output chunk (or until the source ends).
        while self.buffered.len() < STREAM_CHUNK_SAMPLES && !self.drained {
            // `pump` only errors on a resampler failure; treat that as
            // end-of-stream rather than panicking the consumer.
            if self.pump().is_err() {
                self.drained = true;
                break;
            }
        }
        if self.buffered.is_empty() {
            return None;
        }
        Some(self.take_chunk())
    }
}

/// An [`AsyncAudioInput`] that replays a committed 16 kHz mono i16 WAV (or an
/// in-memory buffer) as 100 ms chunks.
///
/// With `realtime = true`, each `next_chunk().await` sleeps for the chunk's
/// audio duration before yielding, so a file drives a streaming backend (or
/// `voice listen`) on the same timeline a live mic would — useful for
/// deterministic streaming tests without hardware.
pub struct FileAsyncAudioInput {
    samples: Vec<i16>,
    cursor: usize,
    chunk_samples: usize,
    realtime: bool,
}

impl FileAsyncAudioInput {
    /// Builds from an in-memory 16 kHz mono i16 buffer.
    pub fn from_samples(samples: Vec<i16>, chunk_samples: usize, realtime: bool) -> Self {
        Self {
            samples,
            cursor: 0,
            chunk_samples: chunk_samples.max(1),
            realtime,
        }
    }

    /// Loads a 16 kHz mono 16-bit PCM WAV (reusing [`VecAudioInput`]'s
    /// validation) and prepares it for chunked async replay.
    pub fn from_wav_path(
        path: impl AsRef<Path>,
        chunk_samples: usize,
        realtime: bool,
    ) -> Result<Self> {
        let mut input = VecAudioInput::from_wav_path(path, chunk_samples.max(1))?;
        let mut samples = Vec::new();
        while let Some(chunk) = input.next_chunk() {
            samples.extend_from_slice(&chunk);
        }
        Ok(Self::from_samples(samples, chunk_samples, realtime))
    }
}

#[async_trait]
impl AsyncAudioInput for FileAsyncAudioInput {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        if self.cursor >= self.samples.len() {
            return None;
        }
        let end = (self.cursor + self.chunk_samples).min(self.samples.len());
        let chunk = self.samples[self.cursor..end].to_vec();
        self.cursor = end;
        if self.realtime {
            #[allow(clippy::cast_precision_loss)]
            let secs = chunk.len() as f64 / TARGET_SAMPLE_RATE;
            tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        }
        Some(chunk)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::audio::FileAudioSource;

    #[test]
    fn mixdown_resample_identity_16k_mono_is_bit_stable() {
        // 16 kHz mono input → resampler identity path → same i16 samples.
        let f32_samples: Vec<f32> = (0..4000)
            .map(|i| ((i % 200) as f32 / 200.0) - 0.5)
            .collect();
        let expected: Vec<i16> = f32_samples.iter().copied().map(f32_to_i16).collect();
        let source = FileAudioSource::from_samples(f32_samples, 16_000, 1, 512);
        let mut adapter = MixdownResampleAudioInput::new(source).unwrap();
        let mut out = Vec::new();
        while let Some(chunk) = adapter.next_chunk() {
            assert!(chunk.len() <= STREAM_CHUNK_SAMPLES);
            out.extend_from_slice(&chunk);
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn mixdown_resample_48k_stereo_downsamples_to_16k_mono() {
        // 1 s of 48 kHz stereo → ~16 kHz mono (≈16000 samples, ±resampler tail).
        let frames = 48_000;
        let mut interleaved = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let v = ((i % 480) as f32 / 480.0) - 0.5;
            interleaved.push(v); // L
            interleaved.push(v); // R (identical → mono == v)
        }
        let source = FileAudioSource::from_samples(interleaved, 48_000, 2, 1024);
        let mut adapter = MixdownResampleAudioInput::new(source).unwrap();
        let mut total = 0usize;
        while let Some(chunk) = adapter.next_chunk() {
            total += chunk.len();
        }
        // 48k→16k is a 3:1 ratio: expect ~16000 output samples (allow sinc tail).
        assert!(
            (15_000..=17_000).contains(&total),
            "expected ~16000 mono 16 kHz samples, got {total}"
        );
    }

    #[tokio::test]
    async fn file_async_input_yields_expected_chunks() {
        let input = FileAsyncAudioInput::from_samples(vec![7; 4_000], STREAM_CHUNK_SAMPLES, false);
        let mut input = input;
        let mut chunks = Vec::new();
        while let Some(c) = input.next_chunk().await {
            chunks.push(c);
        }
        // 4000 / 1600 → 1600, 1600, 800.
        assert_eq!(
            chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![1600, 1600, 800]
        );
        assert!(chunks.iter().flatten().all(|&s| s == 7));
    }

    #[tokio::test]
    async fn file_async_input_realtime_elapses_audio_duration() {
        // 3200 samples @ 16 kHz = 200 ms; realtime mode sleeps each chunk's
        // audio duration, so the loop should take ≳ that (real clock, kept
        // short to stay fast).
        let mut input =
            FileAsyncAudioInput::from_samples(vec![0; 3_200], STREAM_CHUNK_SAMPLES, true);
        let start = std::time::Instant::now();
        while input.next_chunk().await.is_some() {}
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(180),
            "realtime replay should elapse ~200 ms of audio, got {elapsed:?}"
        );
    }
}
