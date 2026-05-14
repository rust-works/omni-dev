//! Kaldi-style FBANK (log-mel filterbank) feature extraction.
//!
//! Produces the 80-dim features that the wespeaker speaker-embedding
//! ONNX model in [`crate::voice::speaker`] consumes. Parameters match
//! sherpa-onnx's `kaldi-native-fbank` defaults for that model:
//!
//! - 16 kHz mono input
//! - 25 ms window (400 samples) / 10 ms hop (160 samples)
//! - Pre-emphasis `0.97`
//! - Hamming window
//! - 80 mel bins, low-freq 20 Hz, high-freq 8000 Hz
//! - Slaney/Kaldi mel scale: `1127 · ln(1 + f/700)`
//! - Log applied to mel energies (`ln(power + 1e-10)`)
//! - Cepstral mean normalisation across all frames of the supplied window
//!
//! The implementation is pure-Rust DSP; the only non-std dep is
//! `rustfft` for the 512-point real FFT.

use std::f32::consts::PI;

use anyhow::{bail, Result};
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

/// Target sample rate: wespeaker is trained on 16 kHz audio.
pub const SAMPLE_RATE: u32 = 16_000;

/// Frame length in milliseconds.
pub const FRAME_LENGTH_MS: f32 = 25.0;

/// Frame shift (hop) in milliseconds.
pub const FRAME_SHIFT_MS: f32 = 10.0;

/// Number of mel bins (also the feature dimension per frame).
pub const NUM_MEL_BINS: usize = 80;

/// FFT size: smallest power of two ≥ frame length (400 → 512).
pub const FFT_SIZE: usize = 512;

/// Mel-filter low-frequency cutoff (Hz). Kaldi default.
pub const LOW_FREQ_HZ: f32 = 20.0;

/// Mel-filter high-frequency cutoff (Hz). Half the sample rate.
pub const HIGH_FREQ_HZ: f32 = (SAMPLE_RATE / 2) as f32;

/// Pre-emphasis coefficient — boost high frequencies before windowing.
pub const PREEMPHASIS: f32 = 0.97;

/// Numerical floor added before `ln` to keep log-mel finite when mel
/// energies are near zero.
const EPSILON: f32 = 1e-10;

/// Builds the triangular mel filterbank as a dense `[num_bins][fft_size/2 + 1]`
/// matrix indexed `[mel_bin][fft_bin]`.
///
/// The same matrix is reused across many windows by
/// [`crate::voice::speaker::WespeakerEmbedder`]; construction cost is
/// paid once per process.
pub fn build_mel_filterbank(
    num_bins: usize,
    fft_size: usize,
    sample_rate: u32,
) -> Result<Vec<Vec<f32>>> {
    if num_bins != NUM_MEL_BINS {
        bail!("wespeaker FBANK requires {NUM_MEL_BINS} mel bins, got {num_bins}");
    }
    let mel_low = hz_to_mel(LOW_FREQ_HZ);
    let mel_high = hz_to_mel(HIGH_FREQ_HZ);
    let num_points = num_bins + 2;
    let mel_points: Vec<f32> = (0..num_points)
        .map(|i| (mel_high - mel_low).mul_add(i as f32 / (num_points - 1) as f32, mel_low))
        .collect();
    let hz_points: Vec<f32> = mel_points.iter().copied().map(mel_to_hz).collect();

    let num_bins_fft = fft_size / 2 + 1;
    let bin_hz: Vec<f32> = (0..num_bins_fft)
        .map(|k| k as f32 * sample_rate as f32 / fft_size as f32)
        .collect();

    let mut filters = vec![vec![0f32; num_bins_fft]; num_bins];
    for m in 0..num_bins {
        let left = hz_points[m];
        let centre = hz_points[m + 1];
        let right = hz_points[m + 2];
        for (k, &f) in bin_hz.iter().enumerate() {
            let w = if f < left || f > right {
                0.0
            } else if f <= centre {
                (f - left) / (centre - left).max(EPSILON)
            } else {
                (right - f) / (right - centre).max(EPSILON)
            };
            filters[m][k] = w.max(0.0);
        }
    }
    Ok(filters)
}

/// Hz → mel via the Slaney/Kaldi formula `1127 · ln(1 + f/700)`.
fn hz_to_mel(hz: f32) -> f32 {
    1127.0 * (hz / 700.0).ln_1p()
}

/// Inverse of [`hz_to_mel`].
fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (mel / 1127.0).exp_m1()
}

/// Hamming window of length `n`.
fn hamming(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (-0.46_f32).mul_add((2.0 * PI * i as f32 / (n - 1) as f32).cos(), 0.54))
        .collect()
}

/// Computes 80-dim Kaldi-style FBANK features for the supplied 16 kHz
/// mono floating-point PCM window. Returns one feature row per frame,
/// already cepstral-mean-normalised across the window.
///
/// Errors if `pcm` is shorter than one frame (400 samples ≈ 25 ms).
pub fn compute_fbank(pcm: &[f32], mel_filters: &[Vec<f32>]) -> Result<Vec<Vec<f32>>> {
    let frame_length = ((FRAME_LENGTH_MS / 1000.0) * SAMPLE_RATE as f32) as usize;
    let frame_shift = ((FRAME_SHIFT_MS / 1000.0) * SAMPLE_RATE as f32) as usize;
    if pcm.len() < frame_length {
        bail!(
            "PCM window has {} samples; need at least {} (one 25 ms frame at 16 kHz)",
            pcm.len(),
            frame_length
        );
    }
    let num_frames = 1 + (pcm.len() - frame_length) / frame_shift;
    let window = hamming(frame_length);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);

    let mut feats: Vec<Vec<f32>> = Vec::with_capacity(num_frames);
    let mut scratch = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    let mut emph = vec![0f32; frame_length];

    for i in 0..num_frames {
        let start = i * frame_shift;
        let frame = &pcm[start..start + frame_length];

        // Pre-emphasis: x[n] - 0.97 · x[n-1]. The kaldi-native-fbank
        // default uses x[0] itself for the n=0 history (no buffer of
        // prior frames carried forward), and that's what we mirror
        // here.
        emph[0] = (-PREEMPHASIS).mul_add(frame[0], frame[0]);
        for n in 1..frame_length {
            emph[n] = (-PREEMPHASIS).mul_add(frame[n - 1], frame[n]);
        }

        // Hamming window, zero-pad to FFT_SIZE.
        for n in 0..FFT_SIZE {
            let v = if n < frame_length {
                emph[n] * window[n]
            } else {
                0.0
            };
            scratch[n] = Complex32::new(v, 0.0);
        }
        fft.process(&mut scratch);

        // Power spectrum, mel projection, log.
        let num_bins_fft = FFT_SIZE / 2 + 1;
        let mut mel = vec![0f32; mel_filters.len()];
        for (m, filter) in mel_filters.iter().enumerate() {
            let mut energy = 0f32;
            for (k, &w) in filter.iter().enumerate().take(num_bins_fft) {
                let c = scratch[k];
                let power = c.re.mul_add(c.re, c.im * c.im);
                energy = w.mul_add(power, energy);
            }
            mel[m] = (energy + EPSILON).ln();
        }
        feats.push(mel);
    }

    // Cepstral mean normalisation across all frames of this window.
    let n = feats.len() as f32;
    let mut mean = vec![0f32; mel_filters.len()];
    for frame in &feats {
        for (m, &v) in frame.iter().enumerate() {
            mean[m] += v;
        }
    }
    for v in &mut mean {
        *v /= n;
    }
    for frame in &mut feats {
        for (m, v) in frame.iter_mut().enumerate() {
            *v -= mean[m];
        }
    }
    Ok(feats)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Generates `secs` seconds of a `freq_hz` sine wave at unit amplitude.
    fn sine(freq_hz: f32, secs: f32) -> Vec<f32> {
        let n = (secs * SAMPLE_RATE as f32) as usize;
        (0..n)
            .map(|i| (2.0 * PI * freq_hz * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn build_mel_filterbank_returns_80_by_257_matrix() {
        let filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE).unwrap();
        assert_eq!(filters.len(), NUM_MEL_BINS);
        assert_eq!(filters[0].len(), FFT_SIZE / 2 + 1);
    }

    #[test]
    fn build_mel_filterbank_rejects_non_80_bins() {
        let err = build_mel_filterbank(64, FFT_SIZE, SAMPLE_RATE).unwrap_err();
        assert!(err.to_string().contains("80 mel bins"), "got: {err}");
    }

    #[test]
    fn build_mel_filterbank_filters_are_non_negative() {
        let filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE).unwrap();
        for (m, filter) in filters.iter().enumerate() {
            for (k, &w) in filter.iter().enumerate() {
                assert!(w >= 0.0, "filter[{m}][{k}] = {w} is negative");
            }
        }
    }

    #[test]
    fn compute_fbank_frame_count_matches_formula() {
        let pcm = sine(1_000.0, 0.5); // 0.5 s → 8000 samples
        let filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE).unwrap();
        let feats = compute_fbank(&pcm, &filters).unwrap();
        let frame_length = 400;
        let frame_shift = 160;
        let expected = 1 + (pcm.len() - frame_length) / frame_shift;
        assert_eq!(feats.len(), expected);
    }

    #[test]
    fn compute_fbank_emits_80_dim_frames() {
        let pcm = sine(1_000.0, 0.5);
        let filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE).unwrap();
        let feats = compute_fbank(&pcm, &filters).unwrap();
        for (i, frame) in feats.iter().enumerate() {
            assert_eq!(frame.len(), NUM_MEL_BINS, "frame {i}: {}", frame.len());
        }
    }

    #[test]
    fn compute_fbank_errors_on_too_short_pcm() {
        let filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE).unwrap();
        let err = compute_fbank(&vec![0.0; 100], &filters).unwrap_err();
        assert!(err.to_string().contains("at least"), "got: {err}");
    }

    #[test]
    fn mel_filter_centres_are_monotonically_increasing_in_hz() {
        // Structural sanity check on the filterbank rather than on the
        // post-CMN feature output: filter m+1's centre frequency must
        // be higher than filter m's. (Post-CMN argmax over a stationary
        // tone is dominated by noise — CMN subtracts the mean-per-bin
        // so all bins are close to zero, and the integration test in
        // #805's `voice_enroll_speaker_test` is the real validation
        // against a trained model.)
        let filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE).unwrap();
        let mut prev_centre_bin = 0usize;
        for (m, filter) in filters.iter().enumerate() {
            let centre_bin = filter
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(i, _)| i)
                .unwrap();
            assert!(
                centre_bin >= prev_centre_bin,
                "mel filter {m} centre bin {centre_bin} < previous {prev_centre_bin}"
            );
            prev_centre_bin = centre_bin;
        }
    }

    #[test]
    fn compute_fbank_cmn_zeros_mean_per_bin() {
        // Cepstral mean normalisation subtracts the per-bin mean, so the
        // post-normalisation mean of each bin should be ≈ 0 across all
        // frames. Pin this invariant — it's what wespeaker's input
        // distribution assumes.
        let pcm = sine(1_000.0, 0.5);
        let filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE).unwrap();
        let feats = compute_fbank(&pcm, &filters).unwrap();
        let nf = feats.len() as f32;
        for m in 0..NUM_MEL_BINS {
            let mean: f32 = feats.iter().map(|f| f[m]).sum::<f32>() / nf;
            assert!(
                mean.abs() < 1e-3,
                "bin {m} post-CMN mean = {mean} (expected ~0)"
            );
        }
    }

    #[test]
    fn hz_mel_round_trip() {
        for &hz in &[20.0_f32, 200.0, 1000.0, 4000.0, 8000.0] {
            let back = mel_to_hz(hz_to_mel(hz));
            assert!((back - hz).abs() < 1e-2, "{hz} -> {back}");
        }
    }
}
