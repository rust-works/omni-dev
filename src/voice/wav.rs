//! Mixdown, resampling, and WAV writing.
//!
//! The write-path half of the capture pipeline:
//!
//! 1. [`mono_mixdown`] collapses N interleaved channels into a single mono
//!    stream by averaging.
//! 2. [`Resampler`] rate-converts mono f32 from the device-native rate to
//!    `16_000` Hz via `rubato`'s sinc interpolator. Identity-passthrough is
//!    used when the input is already 16 kHz so the pipeline stays
//!    bit-exact in that common case.
//! 3. [`WavWriter`] serialises 16 kHz mono f32 to 16-bit signed PCM WAV via
//!    `hound`, with clamp-on-cast to handle resampler overshoot at the
//!    extremes of `[-1.0, 1.0]`.
//!
//! Idle detection and trailing-silence trimming live in
//! [`super::idle`] — they operate on the post-resample 16 kHz stream
//! and are independent of the writer.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hound::{SampleFormat, WavSpec};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler as _, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

/// Target sample rate for the capture pipeline (whisper.cpp convention).
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Number of input frames fed to the resampler per call. Sized to amortise
/// the sinc filter overhead without making the streaming API feel chunky.
/// At 48 kHz input this is ~85 ms of audio.
pub const RESAMPLER_CHUNK_FRAMES: usize = 4096;

/// Averages N interleaved channels into a single mono stream.
///
/// `samples.len()` must be a multiple of `channels`; any trailing partial
/// frame is silently dropped (cpal callbacks never produce partial frames,
/// but the guard lets fixture sources be sloppy without panicking).
///
/// When `channels == 1` the input is returned as-is (no copy round-trip
/// through arithmetic), which preserves bit-exact behaviour for fixtures
/// that are already mono.
#[must_use]
pub fn mono_mixdown(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let channels = channels as usize;
    let frame_count = samples.len() / channels;
    let mut out = Vec::with_capacity(frame_count);
    let inv = 1.0_f32 / channels as f32;
    for frame in samples.chunks_exact(channels) {
        let sum: f32 = frame.iter().copied().sum();
        out.push(sum * inv);
    }
    out
}

/// Streaming resampler from an arbitrary input rate to
/// [`TARGET_SAMPLE_RATE`] (16 kHz), mono.
///
/// The wrapper buffers input frames until it has enough for one
/// fixed-size `rubato` chunk, processes that chunk, and accumulates the
/// variable-length output. Callers feed mono f32 via [`Resampler::push`]
/// and drain a single tail batch via [`Resampler::flush`] at end-of-stream.
///
/// At 16 kHz input the resampler is bypassed entirely — input is forwarded
/// to output verbatim, with zero sinc-filter latency.
pub struct Resampler {
    input_rate: u32,
    inner: Option<Inner>,
}

struct Inner {
    resampler: Async<f32>,
    /// Pending input frames not yet large enough for one chunk.
    pending: Vec<f32>,
    /// Required input frames per call (constant for fixed-input `Async`).
    chunk_frames: usize,
}

impl Resampler {
    /// Builds a resampler that converts `input_rate` Hz mono f32 to
    /// 16 kHz mono f32. Returns an error if the rate is zero or the
    /// `rubato` constructor rejects the configuration.
    pub fn new(input_rate: u32) -> Result<Self> {
        if input_rate == 0 {
            anyhow::bail!("Resampler input rate must be > 0");
        }
        if input_rate == TARGET_SAMPLE_RATE {
            return Ok(Self {
                input_rate,
                inner: None,
            });
        }
        let ratio = f64::from(TARGET_SAMPLE_RATE) / f64::from(input_rate);
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            oversampling_factor: 128,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler = Async::<f32>::new_sinc(
            ratio,
            1.0,
            &params,
            RESAMPLER_CHUNK_FRAMES,
            1,
            FixedAsync::Input,
        )
        .with_context(|| {
            format!("Failed to build resampler for {input_rate} Hz → {TARGET_SAMPLE_RATE} Hz")
        })?;
        Ok(Self {
            input_rate,
            inner: Some(Inner {
                resampler,
                pending: Vec::with_capacity(RESAMPLER_CHUNK_FRAMES * 2),
                chunk_frames: RESAMPLER_CHUNK_FRAMES,
            }),
        })
    }

    /// The configured input sample rate in Hz.
    #[must_use]
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// Output sample rate (constant — always [`TARGET_SAMPLE_RATE`]).
    #[must_use]
    pub fn output_rate(&self) -> u32 {
        TARGET_SAMPLE_RATE
    }

    /// Feeds mono samples and returns any 16 kHz output produced this call.
    ///
    /// Partial input that doesn't fill a full chunk is buffered internally
    /// and emitted on a subsequent `push` or `flush`.
    pub fn push(&mut self, mono: &[f32]) -> Result<Vec<f32>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(mono.to_vec());
        };
        inner.pending.extend_from_slice(mono);
        let mut out = Vec::new();
        while inner.pending.len() >= inner.chunk_frames {
            let chunk = &inner.pending[..inner.chunk_frames];
            let Ok(input_adapter) = InterleavedSlice::new(chunk, 1, inner.chunk_frames) else {
                unreachable!("chunk.len() == 1 * chunk_frames by construction")
            };
            let drained = inner
                .resampler
                .process(&input_adapter, 0, None)
                .context("Resampler chunk processing failed")?;
            inner.pending.drain(..inner.chunk_frames);
            // Mono → interleaved layout is just a flat Vec<f32> of samples.
            out.extend(drained.take_data());
        }
        Ok(out)
    }

    /// Flushes any buffered samples at end-of-stream using `process_partial`
    /// (zero-pads internally). Call at most once after the source is
    /// exhausted.
    pub fn flush(&mut self) -> Result<Vec<f32>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Vec::new());
        };
        let tail = std::mem::take(&mut inner.pending);
        if tail.is_empty() {
            return Ok(Vec::new());
        }
        let Ok(input_adapter) = InterleavedSlice::new(&tail, 1, tail.len()) else {
            unreachable!("tail.len() == 1 * tail.len() by construction")
        };
        let output_capacity = inner.resampler.output_frames_max();
        let mut output_buf = vec![0.0_f32; output_capacity];
        let Ok(mut output_adapter) = InterleavedSlice::new_mut(&mut output_buf, 1, output_capacity)
        else {
            unreachable!("output_buf.len() == 1 * output_capacity by construction")
        };
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(tail.len()),
            active_channels_mask: None,
        };
        let (_in_frames, out_frames) = inner
            .resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
            .context("Resampler flush failed")?;
        output_buf.truncate(out_frames);
        Ok(output_buf)
    }
}

/// Bit depth of the output WAV (whisper.cpp convention).
pub const OUTPUT_BITS_PER_SAMPLE: u16 = 16;

/// Streaming WAV writer that accepts mono 16 kHz f32 samples and emits
/// 16-bit signed PCM.
///
/// Samples are clamped into `[-1.0, 1.0]` before the cast to `i16`.
/// [`WavWriter::finalize`] must be called to flush the header; on drop
/// without finalisation the file is left with an invalid header (size
/// field unwritten) — the orchestrator therefore always calls
/// `finalize`, even on signal-driven shutdown.
pub struct WavWriter {
    inner: hound::WavWriter<BufWriter<File>>,
    path: PathBuf,
    samples_written: u64,
}

impl WavWriter {
    /// Creates a WAV file at `path`, with parent directories created if
    /// they do not exist. The header is written eagerly; the size field
    /// is patched up by [`WavWriter::finalize`].
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create parent directory {}", parent.display())
                })?;
            }
        }
        let spec = WavSpec {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: OUTPUT_BITS_PER_SAMPLE,
            sample_format: SampleFormat::Int,
        };
        let inner = hound::WavWriter::create(&path, spec)
            .with_context(|| format!("Failed to create WAV file at {}", path.display()))?;
        Ok(Self {
            inner,
            path,
            samples_written: 0,
        })
    }

    /// Writes a chunk of mono samples. Values outside `[-1.0, 1.0]` are
    /// clamped (resampler overshoot near 0 dBFS).
    pub fn write_samples(&mut self, samples: &[f32]) -> Result<()> {
        for s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            let scaled = (clamped * f32::from(i16::MAX)).round() as i16;
            self.inner
                .write_sample(scaled)
                .with_context(|| format!("Failed to write sample to {}", self.path.display()))?;
        }
        self.samples_written += samples.len() as u64;
        Ok(())
    }

    /// Number of samples written so far (not counting any clamped value
    /// any differently).
    #[must_use]
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    /// Path the WAV was created at.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flushes the WAV header so the file is playable. Consumes `self` so
    /// double-finalisation cannot occur. Always call this — including on
    /// signal-driven shutdown — or the on-disk file will be malformed.
    pub fn finalize(self) -> Result<()> {
        self.inner
            .finalize()
            .with_context(|| format!("Failed to finalize WAV file at {}", self.path.display()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::f32::consts::TAU;

    #[test]
    fn mono_mixdown_passes_through_mono_untouched() {
        let input = vec![0.1, -0.2, 0.3, -0.4];
        assert_eq!(mono_mixdown(&input, 1), input);
    }

    #[test]
    fn mono_mixdown_averages_stereo_to_zero_for_inverted_signal() {
        // L = 1.0, R = -1.0 → mean = 0.0 for every frame.
        let input = vec![1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
        let out = mono_mixdown(&input, 2);
        assert_eq!(out, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn mono_mixdown_averages_quad_channel() {
        // 4-channel frame averages: (1+2+3+4)/4 = 2.5, (5+6+7+8)/4 = 6.5
        let input = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let out = mono_mixdown(&input, 4);
        assert_eq!(out, vec![2.5, 6.5]);
    }

    #[test]
    fn mono_mixdown_drops_trailing_partial_frame() {
        // 7 samples / 2 channels = 3 full frames, 1 stranded sample.
        let input = vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 99.0];
        let out = mono_mixdown(&input, 2);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn resampler_identity_path_returns_input_verbatim() -> Result<()> {
        let mut r = Resampler::new(TARGET_SAMPLE_RATE)?;
        assert_eq!(r.input_rate(), TARGET_SAMPLE_RATE);
        assert_eq!(r.output_rate(), TARGET_SAMPLE_RATE);
        let input: Vec<f32> = (0..100).map(|i| (i as f32 / 100.0) - 0.5).collect();
        let out = r.push(&input)?;
        assert_eq!(out, input);
        let flushed = r.flush()?;
        assert!(flushed.is_empty());
        Ok(())
    }

    #[test]
    fn resampler_rejects_zero_input_rate() {
        let err = Resampler::new(0).err().expect("must reject zero rate");
        assert!(err.to_string().contains("> 0"));
    }

    fn sine_wave(rate: u32, freq_hz: f32, duration_s: f32, amplitude: f32) -> Vec<f32> {
        let n = (rate as f32 * duration_s) as usize;
        (0..n)
            .map(|i| amplitude * (TAU * freq_hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }

    #[test]
    fn resampler_48k_to_16k_preserves_signal_rms() -> Result<()> {
        // 2 s of a 440 Hz sine at amplitude 0.5 — well below Nyquist at both rates.
        let input = sine_wave(48_000, 440.0, 2.0, 0.5);
        let mut r = Resampler::new(48_000)?;
        let mut output = r.push(&input)?;
        output.extend(r.flush()?);
        // 2 s @ 16 kHz ≈ 32_000 samples. The flush() call zero-pads any
        // residual input to one full rubato chunk, so the output can run
        // up to one chunk's worth of resampled frames long. Trailing-
        // silence trim (step 4) is responsible for cleaning that up.
        let expected_len: usize = 32_000;
        let max_overrun = (RESAMPLER_CHUNK_FRAMES as f64 * 16_000.0 / 48_000.0).ceil() as usize;
        assert!(
            output.len() >= expected_len.saturating_sub(256),
            "output too short: got {}, expected ≥ {}",
            output.len(),
            expected_len - 256
        );
        assert!(
            output.len() <= expected_len + max_overrun + 256,
            "output too long: got {}, expected ≤ {}",
            output.len(),
            expected_len + max_overrun + 256
        );
        // Ignore the first ~50 ms transient (sinc filter warm-up); compare RMS over the steady-state.
        let warmup = 800; // 50 ms @ 16k
        let in_rms = rms(&input);
        let out_rms = rms(&output[warmup..]);
        assert!(
            (in_rms - out_rms).abs() < 0.02,
            "RMS drift too large: in={in_rms}, out={out_rms}"
        );
        Ok(())
    }

    #[test]
    fn resampler_chunked_and_one_shot_match() -> Result<()> {
        // Two resamplers, identical input, different chunking — outputs must agree.
        let input = sine_wave(48_000, 261.6, 1.0, 0.3);
        let mut one_shot = Resampler::new(48_000)?;
        let mut a = one_shot.push(&input)?;
        a.extend(one_shot.flush()?);

        let mut chunked = Resampler::new(48_000)?;
        let mut b = Vec::new();
        for chunk in input.chunks(977) {
            // odd chunk size — exercises boundary handling
            b.extend(chunked.push(chunk)?);
        }
        b.extend(chunked.flush()?);

        assert_eq!(a.len(), b.len(), "chunked and one-shot length disagree");
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert!(
                (x - y).abs() < 1e-5,
                "sample {i}: chunked={y}, one-shot={x}"
            );
        }
        Ok(())
    }

    #[test]
    fn wav_writer_round_trips_samples() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let path = tmp.path().join("out.wav");
        let mut writer = WavWriter::create(&path)?;
        let samples: Vec<f32> = (0..1000)
            .map(|i| (TAU * 440.0 * i as f32 / 16_000.0).sin() * 0.25)
            .collect();
        writer.write_samples(&samples)?;
        assert_eq!(writer.samples_written(), 1000);
        writer.finalize()?;

        let mut reader = hound::WavReader::open(&path)?;
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(spec.bits_per_sample, OUTPUT_BITS_PER_SAMPLE);
        assert_eq!(spec.sample_format, SampleFormat::Int);
        let decoded: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| f32::from(s.unwrap()) / f32::from(i16::MAX))
            .collect();
        assert_eq!(decoded.len(), 1000);
        // Round-trip drift bounded by 1 lsb of 16-bit quantisation.
        for (i, (orig, got)) in samples.iter().zip(decoded.iter()).enumerate() {
            assert!(
                (orig - got).abs() < 1.0 / f32::from(i16::MAX),
                "sample {i}: orig={orig}, got={got}"
            );
        }
        Ok(())
    }

    #[test]
    fn wav_writer_clamps_samples_to_int_range() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let path = tmp.path().join("clamp.wav");
        let mut writer = WavWriter::create(&path)?;
        // Values outside [-1.0, 1.0] should clamp, not wrap.
        writer.write_samples(&[2.0, -2.0, 0.5, -0.5])?;
        writer.finalize()?;

        let mut reader = hound::WavReader::open(&path)?;
        let decoded: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(decoded[0], i16::MAX, "2.0 should clamp to i16::MAX");
        // -1.0 * i16::MAX = -32767, not i16::MIN (-32768) — we scale by
        // i16::MAX, not i16::MIN.abs(), so symmetric range.
        assert_eq!(decoded[1], -i16::MAX, "-2.0 should clamp to -i16::MAX");
        // 0.5 * 32767 ≈ 16383, with rounding.
        assert!((decoded[2] - 16384).abs() <= 1);
        assert!((decoded[3] + 16384).abs() <= 1);
        Ok(())
    }

    #[test]
    fn wav_writer_creates_parent_dirs() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let nested = tmp.path().join("a").join("b").join("c");
        let path = nested.join("nested.wav");
        let writer = WavWriter::create(&path)?;
        writer.finalize()?;
        assert!(path.exists());
        Ok(())
    }

    #[test]
    fn mono_mixdown_passes_through_zero_channels_unchanged() {
        // channels == 0 hits the `<= 1` branch and returns the input as-is;
        // documents the no-op behaviour rather than letting it silently
        // change.
        let input = vec![0.1, 0.2, 0.3];
        assert_eq!(mono_mixdown(&input, 0), input);
    }

    #[test]
    fn resampler_push_empty_returns_empty() -> Result<()> {
        let mut r = Resampler::new(48_000)?;
        let out = r.push(&[])?;
        assert!(out.is_empty());
        Ok(())
    }

    #[test]
    fn resampler_flush_with_no_pending_input_is_empty() -> Result<()> {
        // Identity path: no inner resampler, flush always returns empty.
        let mut r = Resampler::new(TARGET_SAMPLE_RATE)?;
        assert!(r.flush()?.is_empty());
        Ok(())
    }

    #[test]
    fn resampler_identity_push_empty_returns_empty() -> Result<()> {
        let mut r = Resampler::new(TARGET_SAMPLE_RATE)?;
        let out = r.push(&[])?;
        assert!(out.is_empty());
        Ok(())
    }
}
