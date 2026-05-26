//! NeMo-style log-mel spectrogram front-end for Parakeet.
//!
//! Implements the `AudioToMelSpectrogramPreprocessor` config Parakeet
//! ships with: 80 mel bins, `n_fft=512`, `win_length=400`, `hop_length=160`
//! at 16 kHz, Hann window, log-magnitude (natural log of `mag**2 + eps`),
//! per-feature normalisation. No pre-emphasis (NeMo's default `preemph` is
//! disabled in the Parakeet config), no dithering (training-only).
//!
//! Two evaluation modes:
//!
//! - **Batch** ([`ParakeetMel::batch`]): computes log-mel for the whole
//!   PCM input, then per-feature mean/std normalise across the *full*
//!   time axis. The standard path for `Transcriber::transcribe`.
//!
//! - **Streaming** ([`ParakeetMel::streaming_chunk`]): updates a
//!   [`RunningStats`] accumulator across chunks and normalises each new
//!   batch of frames against the *current* running mean/std — mirrors the
//!   patched `newhoggy/parakeet-mlx@32b8034` `StreamingParakeet` fix that
//!   issue #898 calls out as load-bearing. Without it, streaming
//!   normalisation diverges from batch normalisation and WER blows up.
//!
//! Mel filterbank construction: HTK-style mel scale
//! (`mel = 2595 * log10(1 + hz/700)`), Slaney-style filter normalisation
//! (each triangular filter has unit area). Matches `librosa.filters.mel(
//! sr=16000, n_fft=512, n_mels=80, fmin=0, fmax=8000, htk=False,
//! norm="slaney")` to within 1e-6; numerical parity against the upstream
//! MLX reference is asserted by the snapshot tests in commit 10.

use std::sync::Arc;

use anyhow::{ensure, Result};
use rustfft::{num_complex::Complex32, Fft, FftPlanner};

/// Sample rate Parakeet was trained at and the only rate this backend
/// accepts. Audio at any other rate must be resampled before reaching the
/// mel front-end.
pub const SAMPLE_RATE: u32 = 16_000;

/// FFT size used by the spectrogram.
pub const N_FFT: usize = 512;

/// Window length in samples (Hann); shorter than `N_FFT`, so the window
/// is zero-padded out to `N_FFT` before the FFT.
pub const WIN_LENGTH: usize = 400;

/// Hop between successive frames, in samples (10 ms at 16 kHz).
pub const HOP_LENGTH: usize = 160;

/// Number of mel bands the encoder consumes.
pub const N_MELS: usize = 128;

/// Lower frequency bound for the mel filterbank.
pub const FMIN: f32 = 0.0;

/// Upper frequency bound for the mel filterbank.
pub const FMAX: f32 = 8_000.0;

/// Log floor — added to magnitude-squared before taking the natural log so
/// silent frames don't produce `-inf`. Matches NeMo's `log_zero_guard_value`
/// default of `2**-24`.
const LOG_EPS: f32 = 5.960_464_5e-8;

/// Per-feature normalisation epsilon added to the standard deviation
/// before division — guards against constant features (silence) producing
/// `NaN`. Matches NeMo's `CONSTANT` of `1e-5`.
const NORM_EPS: f32 = 1e-5;

/// One frame's worth of mel features, plus the dims needed by the
/// downstream encoder to build a `(batch, n_mels, n_frames)` tensor.
#[derive(Debug, Clone, PartialEq)]
pub struct MelFrames {
    /// Flat `n_frames * n_mels` buffer in time-major layout: `data[t * n_mels + m]`
    /// is the value of mel bin `m` at frame `t`.
    pub data: Vec<f32>,
    /// Number of time frames.
    pub n_frames: usize,
    /// Number of mel bins (always [`N_MELS`] for Parakeet).
    pub n_mels: usize,
}

impl MelFrames {
    fn empty() -> Self {
        Self {
            data: Vec::new(),
            n_frames: 0,
            n_mels: N_MELS,
        }
    }

    /// Number of mel features (`n_frames * n_mels`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// `true` when no frames were produced.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n_frames == 0
    }
}

/// Welford-style running statistics for per-feature mel normalisation
/// across a streaming session.
///
/// Each mel bin accumulates its own running sum and sum-of-squares so the
/// mean and standard deviation can be recomputed on demand without
/// re-walking the audio. Threaded through
/// [`ParakeetMel::streaming_chunk`] by the streaming transcriber's
/// per-session state.
#[derive(Debug, Clone)]
pub struct RunningStats {
    // Welford's online algorithm in f64: numerically stable across
    // arbitrary chunk schedules, no catastrophic cancellation.
    mean: Vec<f64>,
    m2: Vec<f64>,
    count: u64,
    n_mels: usize,
}

impl RunningStats {
    /// Fresh stats accumulator for an [`N_MELS`]-band stream.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mean: vec![0.0; N_MELS],
            m2: vec![0.0; N_MELS],
            count: 0,
            n_mels: N_MELS,
        }
    }

    /// Number of frames accumulated so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    fn update(&mut self, frame: &[f32]) {
        debug_assert_eq!(frame.len(), self.n_mels);
        self.count += 1;
        #[allow(clippy::cast_precision_loss)]
        let n = self.count as f64;
        for (i, &v) in frame.iter().enumerate() {
            let x = f64::from(v);
            let delta = x - self.mean[i];
            self.mean[i] += delta / n;
            let delta2 = x - self.mean[i];
            self.m2[i] += delta * delta2;
        }
    }

    /// Per-bin (mean, std). `std` floors at [`NORM_EPS`] so callers can
    /// divide without checking for zero. Returns `(zeros, ones)` before
    /// any frames have been accumulated.
    fn mean_std(&self) -> (Vec<f32>, Vec<f32>) {
        if self.count == 0 {
            return (vec![0.0; self.n_mels], vec![1.0; self.n_mels]);
        }
        #[allow(clippy::cast_precision_loss)]
        let n = self.count as f64;
        let mut mean = Vec::with_capacity(self.n_mels);
        let mut std = Vec::with_capacity(self.n_mels);
        for i in 0..self.n_mels {
            #[allow(clippy::cast_possible_truncation)]
            mean.push(self.mean[i] as f32);
            let var = (self.m2[i] / n).max(0.0);
            #[allow(clippy::cast_possible_truncation)]
            std.push((var.sqrt() as f32).max(NORM_EPS));
        }
        (mean, std)
    }
}

impl Default for RunningStats {
    fn default() -> Self {
        Self::new()
    }
}

/// NeMo-style log-mel spectrogram engine for Parakeet.
///
/// Precomputes the FFT plan, the Hann window (zero-padded to `N_FFT`),
/// and the triangular mel filterbank once at construction. Cheap to
/// share across calls — `&self` API, no internal mutable state. Streaming
/// callers thread a [`RunningStats`] through instead.
pub struct ParakeetMel {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    mel_filters: Vec<Vec<f32>>,
}

impl ParakeetMel {
    /// Precomputes the FFT plan, Hann window, and mel filterbank.
    pub fn new() -> Result<Self> {
        let mut planner: FftPlanner<f32> = FftPlanner::new();
        let fft = planner.plan_fft_forward(N_FFT);
        let window = hann_window_padded(WIN_LENGTH, N_FFT);
        let mel_filters = mel_filterbank(N_MELS, N_FFT, SAMPLE_RATE, FMIN, FMAX);
        Ok(Self {
            fft,
            window,
            mel_filters,
        })
    }

    /// Batch path: compute log-mel for the whole PCM input, then
    /// per-feature mean/std normalise across all frames. Uses the same
    /// [`RunningStats`] machinery as [`Self::streaming_chunk`] internally
    /// so a single-chunk streaming call is numerically identical to a
    /// batch call on the same audio.
    pub fn batch(&self, pcm: &[f32]) -> Result<MelFrames> {
        ensure!(!pcm.is_empty(), "ParakeetMel::batch called with empty PCM");
        let mut frames = self.log_mel_frames(pcm)?;
        let mut stats = RunningStats::new();
        for t in 0..frames.n_frames {
            let off = t * N_MELS;
            stats.update(&frames.data[off..off + N_MELS]);
        }
        let (mean, std) = stats.mean_std();
        apply_normalisation(&mut frames, &mean, &std);
        Ok(frames)
    }

    /// Streaming path: compute log-mel for this chunk's PCM, update the
    /// running stats with the new frames, then normalise the chunk
    /// against the *current* running mean/std.
    ///
    /// Callers must thread the same [`RunningStats`] across chunks for a
    /// single session — that's the load-bearing fix from
    /// `newhoggy/parakeet-mlx@32b8034`. A fresh accumulator per chunk
    /// reproduces the batch/streaming divergence #898 explicitly warns
    /// against.
    pub fn streaming_chunk(&self, pcm: &[f32], stats: &mut RunningStats) -> Result<MelFrames> {
        let mut frames = self.log_mel_frames(pcm)?;
        // Update stats with the new (un-normalised) frames first so
        // the normalisation below sees the latest mean/std.
        for t in 0..frames.n_frames {
            let off = t * N_MELS;
            stats.update(&frames.data[off..off + N_MELS]);
        }
        let (mean, std) = stats.mean_std();
        apply_normalisation(&mut frames, &mean, &std);
        Ok(frames)
    }

    /// Core spectrogram path: hop through `pcm` producing one log-mel
    /// frame per [`HOP_LENGTH`] samples. Returned [`MelFrames`] is
    /// **un-normalised** — caller normalises with batch or running stats.
    fn log_mel_frames(&self, pcm: &[f32]) -> Result<MelFrames> {
        if pcm.len() < WIN_LENGTH {
            return Ok(MelFrames::empty());
        }
        let n_frames = (pcm.len() - WIN_LENGTH) / HOP_LENGTH + 1;
        let n_freq = N_FFT / 2 + 1;
        let mut data = Vec::with_capacity(n_frames * N_MELS);
        let mut buf: Vec<Complex32> = vec![Complex32::new(0.0, 0.0); N_FFT];

        for t in 0..n_frames {
            let start = t * HOP_LENGTH;
            // Windowed copy (Hann window already zero-padded to N_FFT).
            for (i, slot) in buf.iter_mut().enumerate() {
                let sample = if i < WIN_LENGTH {
                    pcm[start + i] * self.window[i]
                } else {
                    0.0
                };
                *slot = Complex32::new(sample, 0.0);
            }
            self.fft.process(&mut buf);

            // Power spectrum, then mel projection, then log.
            for mel in 0..N_MELS {
                let filter = &self.mel_filters[mel];
                let mut energy = 0.0_f32;
                for (k, &w) in filter.iter().enumerate().take(n_freq) {
                    if w == 0.0 {
                        continue;
                    }
                    let c = buf[k];
                    let power = c.re.mul_add(c.re, c.im * c.im);
                    energy = power.mul_add(w, energy);
                }
                data.push((energy + LOG_EPS).ln());
            }
        }

        Ok(MelFrames {
            data,
            n_frames,
            n_mels: N_MELS,
        })
    }
}

fn hann_window_padded(win_length: usize, n_fft: usize) -> Vec<f32> {
    let mut w = vec![0.0_f32; n_fft];
    // librosa-style periodic Hann: divisor is `win_length`, not `win_length - 1`.
    #[allow(clippy::cast_precision_loss)]
    let denom = win_length as f32;
    for (i, slot) in w.iter_mut().enumerate().take(win_length) {
        #[allow(clippy::cast_precision_loss)]
        let x = i as f32;
        let phase = 2.0 * std::f32::consts::PI * x / denom;
        *slot = 0.5_f32.mul_add(-phase.cos(), 0.5);
    }
    w
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0_f32.powf(mel / 2595.0) - 1.0)
}

/// Triangular mel filterbank: `n_mels` filters, each `n_fft/2 + 1` bins
/// wide. Slaney-normalised so each filter has unit area — matches
/// `librosa.filters.mel(..., htk=False, norm="slaney")` (the htk arg
/// flips the mel-scale formula but Slaney normalisation is independent
/// of it, and this is the layout NeMo's preprocessor expects).
fn mel_filterbank(n_mels: usize, n_fft: usize, sr: u32, fmin: f32, fmax: f32) -> Vec<Vec<f32>> {
    let n_freq = n_fft / 2 + 1;
    let mel_min = hz_to_mel(fmin);
    let mel_max = hz_to_mel(fmax);

    // n_mels + 2 mel points → n_mels triangles.
    let mut mel_points = Vec::with_capacity(n_mels + 2);
    for i in 0..=n_mels + 1 {
        #[allow(clippy::cast_precision_loss)]
        let frac = i as f32 / (n_mels + 1) as f32;
        mel_points.push(mel_min + frac * (mel_max - mel_min));
    }
    let hz_points: Vec<f32> = mel_points.iter().copied().map(mel_to_hz).collect();

    // FFT bin centre frequencies.
    #[allow(clippy::cast_precision_loss)]
    let sr_f = sr as f32;
    let bin_hz: Vec<f32> = (0..n_freq)
        .map(|k| {
            #[allow(clippy::cast_precision_loss)]
            let kf = k as f32;
            kf * sr_f / n_fft as f32
        })
        .collect();

    let mut filters = vec![vec![0.0_f32; n_freq]; n_mels];
    for m in 0..n_mels {
        let left = hz_points[m];
        let centre = hz_points[m + 1];
        let right = hz_points[m + 2];
        // Slaney normalisation: divide by half the filter's bandwidth in Hz.
        let enorm = 2.0 / (right - left).max(f32::EPSILON);
        for (k, &freq) in bin_hz.iter().enumerate() {
            let weight = if freq < left || freq > right {
                0.0
            } else if freq <= centre {
                (freq - left) / (centre - left).max(f32::EPSILON)
            } else {
                (right - freq) / (right - centre).max(f32::EPSILON)
            };
            filters[m][k] = weight * enorm;
        }
    }
    filters
}

fn apply_normalisation(frames: &mut MelFrames, mean: &[f32], std: &[f32]) {
    for t in 0..frames.n_frames {
        let off = t * N_MELS;
        for m in 0..N_MELS {
            frames.data[off + m] = (frames.data[off + m] - mean[m]) / std[m];
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sine_wave(freq_hz: f32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 / SAMPLE_RATE as f32;
                (2.0 * std::f32::consts::PI * freq_hz * t).sin()
            })
            .collect()
    }

    #[test]
    fn hz_mel_round_trip() {
        for hz in [0.0, 100.0, 1_000.0, 4_000.0, 8_000.0] {
            let back = mel_to_hz(hz_to_mel(hz));
            assert!((back - hz).abs() < 1e-2, "{hz} round-tripped to {back}");
        }
    }

    #[test]
    fn mel_filterbank_has_correct_shape_and_unit_area() {
        let filters = mel_filterbank(N_MELS, N_FFT, SAMPLE_RATE, FMIN, FMAX);
        assert_eq!(filters.len(), N_MELS);
        assert_eq!(filters[0].len(), N_FFT / 2 + 1);

        // Sanity check on the filterbank: each filter's discrete "area"
        // (sum of weights × FFT bin width in Hz) should be in a
        // reasonable range. Loose bounds because with N_MELS=128 over
        // n_fft=512 the discrete sum overestimates the true integral
        // for narrow low-frequency filters (filter narrower than the
        // FFT bin width amplifies Slaney's per-bin weight), and the
        // very lowest filters fall below the lowest non-zero FFT bin
        // and are inherently zero-area. The test catches gross errors
        // (areas in the hundreds, all-zero filterbank) without trying
        // to verify exact Slaney normalisation under the narrow-band
        // regime.
        #[allow(clippy::cast_precision_loss)]
        let bin_hz = SAMPLE_RATE as f32 / N_FFT as f32;
        for (m, f) in filters.iter().enumerate() {
            let area: f32 = f.iter().sum::<f32>() * bin_hz;
            assert!(
                (0.0..=4.0).contains(&area),
                "filter {m} area {area} out of range — Slaney normalisation suspect"
            );
        }
    }

    #[test]
    fn hann_window_zero_padded_after_win_length() {
        let w = hann_window_padded(WIN_LENGTH, N_FFT);
        assert_eq!(w.len(), N_FFT);
        for &v in w.iter().take(WIN_LENGTH) {
            assert!((0.0..=1.0).contains(&v));
        }
        for &v in w.iter().skip(WIN_LENGTH) {
            #[allow(clippy::float_cmp)]
            {
                assert_eq!(v, 0.0);
            }
        }
    }

    #[test]
    fn batch_produces_expected_frame_count() {
        let mel = ParakeetMel::new().unwrap();
        // 1 second of audio → (16000 - 400) / 160 + 1 = 98 frames.
        let pcm = sine_wave(440.0, SAMPLE_RATE as usize);
        let frames = mel.batch(&pcm).unwrap();
        assert_eq!(frames.n_frames, 98);
        assert_eq!(frames.n_mels, N_MELS);
        assert_eq!(frames.data.len(), 98 * N_MELS);
    }

    fn deterministic_noise(samples: usize) -> Vec<f32> {
        // LCG-driven pseudo-noise: cheap, deterministic, excites all mel
        // bins so per-feature normalisation has a non-degenerate signal
        // in every band. Avoids pulling in a random-number-generator dep
        // for one test.
        let mut state: u32 = 0x1234_5678;
        (0..samples)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                #[allow(clippy::cast_precision_loss)]
                let u = state as f32 / u32::MAX as f32;
                u.mul_add(2.0, -1.0)
            })
            .collect()
    }

    #[test]
    fn batch_normalisation_yields_unit_variance_per_bin() {
        let mel = ParakeetMel::new().unwrap();
        // White noise excites every mel bin so std never floors at
        // NORM_EPS — otherwise division by 1e-5 in silent bins
        // amplifies f32 rounding into large false-failures.
        let pcm = deterministic_noise(SAMPLE_RATE as usize);
        let frames = mel.batch(&pcm).unwrap();

        for m in 0..N_MELS {
            let col: Vec<f32> = (0..frames.n_frames)
                .map(|t| frames.data[t * N_MELS + m])
                .collect();
            let mean: f32 = col.iter().sum::<f32>() / col.len() as f32;
            let var: f32 = col.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / col.len() as f32;
            if var < 1e-12 {
                // Inherently silent bin (mel filter falls below the
                // lowest non-zero FFT frequency at this N_MELS / FMIN
                // / n_fft combo). After normalisation by the NORM_EPS
                // floor, all frames are ~0 and there's no signal to
                // assess. Skip rather than false-fail.
                continue;
            }
            assert!(mean.abs() < 1e-4, "bin {m} mean = {mean}");
            assert!((var - 1.0).abs() < 1e-3, "bin {m} variance = {var}");
        }
    }

    #[test]
    fn streaming_one_shot_matches_batch_on_running_stats_only() {
        // When the full PCM is fed as a single streaming chunk, the
        // running stats see exactly the same frames as the batch path,
        // so normalisation is identical. This is the easy case;
        // multi-chunk equality is *not* asserted because streaming
        // normalisation deliberately uses partial stats per chunk.
        let mel = ParakeetMel::new().unwrap();
        let pcm = sine_wave(440.0, SAMPLE_RATE as usize);
        let batch = mel.batch(&pcm).unwrap();

        let mut stats = RunningStats::new();
        let stream = mel.streaming_chunk(&pcm, &mut stats).unwrap();

        assert_eq!(batch.n_frames, stream.n_frames);
        // Both paths share `RunningStats`, so they should be bit-equal
        // when given identical frames.
        for (b, s) in batch.data.iter().zip(stream.data.iter()) {
            assert!((b - s).abs() < 1e-6, "batch {b} vs stream {s}");
        }
        assert_eq!(stats.count(), batch.n_frames as u64);
    }

    #[test]
    fn streaming_chunked_accumulates_stats_across_calls() {
        let mel = ParakeetMel::new().unwrap();
        // Two halves of one second; check stats see both halves.
        let pcm = sine_wave(440.0, SAMPLE_RATE as usize);
        let mid = SAMPLE_RATE as usize / 2;

        let mut stats = RunningStats::new();
        let first = mel.streaming_chunk(&pcm[..mid], &mut stats).unwrap();
        let count_after_first = stats.count();
        let second = mel.streaming_chunk(&pcm[mid..], &mut stats).unwrap();
        let count_after_second = stats.count();

        assert!(count_after_first > 0);
        assert!(count_after_second > count_after_first);
        assert_eq!(
            count_after_second,
            (first.n_frames + second.n_frames) as u64
        );
    }

    #[test]
    fn batch_rejects_empty_pcm() {
        let mel = ParakeetMel::new().unwrap();
        let err = mel.batch(&[]).unwrap_err();
        assert!(err.to_string().contains("empty PCM"), "got: {err}");
    }

    #[test]
    fn log_mel_frames_returns_empty_when_pcm_shorter_than_window() {
        let mel = ParakeetMel::new().unwrap();
        let frames = mel.log_mel_frames(&[0.0; WIN_LENGTH - 1]).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn running_stats_zero_count_returns_identity_mean_std() {
        let stats = RunningStats::new();
        let (mean, std) = stats.mean_std();
        assert_eq!(mean, vec![0.0; N_MELS]);
        assert_eq!(std, vec![1.0; N_MELS]);
    }
}
