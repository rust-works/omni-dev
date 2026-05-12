//! Mixdown, resampling, idle detection, and WAV writing.
//!
//! The write-path half of the capture pipeline:
//!
//! 1. [`mono_mixdown`] collapses N interleaved channels into a single mono
//!    stream by averaging.
//! 2. [`Resampler`] rate-converts mono f32 from the device-native rate to
//!    `16_000` Hz via `rubato`'s sinc interpolator. Identity-passthrough is
//!    used when the input is already 16 kHz so the pipeline stays
//!    bit-exact in that common case.
//!
//! The idle detector, trailing-silence trim, and `hound` WAV writer are
//! added incrementally in steps 4–5.

use anyhow::{Context, Result};
use rubato::{
    Resampler as _, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
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
    resampler: SincFixedIn<f32>,
    /// Pending input frames not yet large enough for one chunk.
    pending: Vec<f32>,
    /// Required input frames per call (constant for `SincFixedIn`).
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
        let resampler = SincFixedIn::<f32>::new(ratio, 1.0, params, RESAMPLER_CHUNK_FRAMES, 1)
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
            let drained = inner
                .resampler
                .process(&[&inner.pending[..inner.chunk_frames]], None)
                .context("Resampler chunk processing failed")?;
            inner.pending.drain(..inner.chunk_frames);
            if let Some(channel) = drained.into_iter().next() {
                out.extend_from_slice(&channel);
            }
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
        let input: Option<&[Vec<f32>]> = if tail.is_empty() {
            None
        } else {
            Some(std::slice::from_ref(&tail))
        };
        let drained = inner
            .resampler
            .process_partial(input, None)
            .context("Resampler flush failed")?;
        Ok(drained.into_iter().next().unwrap_or_default())
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
}
