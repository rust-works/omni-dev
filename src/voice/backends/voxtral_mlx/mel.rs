//! Log-mel front-end — a port of `mlx-audio`'s `voxtral_realtime/audio.py`
//! (which mirrors vLLM / `mistral_common`).
//!
//! Pipeline: pad → reflect-center-pad → framed STFT (periodic Hann, n_fft 400,
//! hop 160) → power spectrum → Slaney mel filter bank (128 bins, 0–8000 Hz) →
//! `log10`, clamp, `(x + 4) / 4` → `[128, frames]`. Computed on the host in F32
//! with `rustfft`; it is cheap relative to the transformer and keeps the exact
//! numeric recipe under our control (accuracy matters for WER).

use std::f32::consts::PI;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use super::config::AudioConfig;

const N_FFT: usize = 400;
const HOP: usize = 160;
const SAMPLE_RATE: f32 = 16_000.0;
const N_MELS: usize = 128;
const F_MAX: f32 = 8_000.0;
/// Samples of raw audio per audio token (`16000 / 12.5`).
pub const RAW_AUDIO_PER_TOK: usize = 1_280;

/// Slaney `hz → mel` (HTK = false): linear below 1 kHz, logarithmic above.
fn hz_to_mel(hz: f32) -> f32 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1_000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = (6.4_f32).ln() / 27.0;
    if hz >= min_log_hz {
        min_log_mel + (hz / min_log_hz).ln() / logstep
    } else {
        hz / f_sp
    }
}

/// Slaney `mel → hz`, inverse of [`hz_to_mel`].
fn mel_to_hz(mel: f32) -> f32 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1_000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = (6.4_f32).ln() / 27.0;
    if mel >= min_log_mel {
        min_log_hz * (logstep * (mel - min_log_mel)).exp()
    } else {
        f_sp * mel
    }
}

/// Builds the Slaney-normalized mel filter bank as `[mel][freq]` (128 × 201),
/// matching `librosa.filters.mel(sr, n_fft, n_mels, fmin=0, fmax=8000,
/// norm="slaney", htk=False)`.
fn mel_filter_bank() -> Vec<[f32; N_FFT / 2 + 1]> {
    const N_FREQ: usize = N_FFT / 2 + 1; // 201
    let fft_freqs: Vec<f32> = (0..N_FREQ)
        .map(|k| k as f32 * SAMPLE_RATE / N_FFT as f32)
        .collect();

    // n_mels + 2 mel-spaced band edges, in Hz.
    let mel_min = hz_to_mel(0.0);
    let mel_max = hz_to_mel(F_MAX);
    let edges: Vec<f32> = (0..N_MELS + 2)
        .map(|i| {
            let mel = mel_min + (mel_max - mel_min) * i as f32 / (N_MELS + 1) as f32;
            mel_to_hz(mel)
        })
        .collect();
    let fdiff: Vec<f32> = edges.windows(2).map(|w| w[1] - w[0]).collect();

    let mut bank = vec![[0.0_f32; N_FREQ]; N_MELS];
    for (m, row) in bank.iter_mut().enumerate() {
        let enorm = 2.0 / (edges[m + 2] - edges[m]);
        for (k, freq) in fft_freqs.iter().enumerate() {
            let lower = -(edges[m] - freq) / fdiff[m];
            let upper = (edges[m + 2] - freq) / fdiff[m + 1];
            row[k] = lower.min(upper).max(0.0) * enorm;
        }
    }
    bank
}

/// Pads audio for offline transcription (port of `_pad_audio_streaming`): a left
/// pad of `n_left` tokens of silence, plus a right pad that first aligns the
/// length to a token boundary then adds `n_right` tokens of silence.
pub fn pad_audio(samples: &[f32], n_left: usize, n_right: usize) -> Vec<f32> {
    let mult = RAW_AUDIO_PER_TOK;
    let align = (mult - (samples.len() % mult)) % mult;
    let left = n_left * mult;
    let right = align + n_right * mult;
    let mut out = vec![0.0_f32; left + samples.len() + right];
    out[left..left + samples.len()].copy_from_slice(samples);
    out
}

/// Reflect-pads `x` by `pad` on each side (NumPy `mode="reflect"`: the edge
/// sample is not repeated).
fn reflect_pad(x: &[f32], pad: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(x.len() + 2 * pad);
    for i in 0..pad {
        out.push(x[pad - i]);
    }
    out.extend_from_slice(x);
    let n = x.len();
    for i in 0..pad {
        out.push(x[n - 2 - i]);
    }
    out
}

/// Computes the `[128, frames]` log-mel spectrogram (row-major, `mel[m*frames + t]`)
/// for already-silence-padded `samples`, plus the frame count. Mirrors
/// `compute_mel_spectrogram`: periodic Hann, reflect center-pad, power spectrum,
/// drop the last frame, Slaney mel, `log10`/clamp/`(x+4)/4`.
// Plain multiply-add (not `mul_add`/FMA) is deliberate: the reference computes
// the power spectrum and mel projection without fused operations, and matching
// its rounding keeps the front-end numerically faithful (accuracy → WER).
#[allow(clippy::suboptimal_flops)]
fn log_mel(samples: &[f32], global_log_mel_max: f32) -> (Vec<f32>, usize) {
    const N_FREQ: usize = N_FFT / 2 + 1;
    // Periodic Hann window (divide by N, not N-1).
    let window: Vec<f32> = (0..N_FFT)
        .map(|n| 0.5 * (1.0 - (2.0 * PI * n as f32 / N_FFT as f32).cos()))
        .collect();

    let padded = reflect_pad(samples, N_FFT / 2);
    let n_frames = 1 + (padded.len() - N_FFT) / HOP;
    let bank = mel_filter_bank();

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(N_FFT);

    // Power spectrum per frame, dropping the last frame → out_frames.
    let out_frames = n_frames - 1;
    // mel[m][t]
    let mut mel = vec![0.0_f32; N_MELS * out_frames];
    let min_val = global_log_mel_max - 8.0;
    let mut buf = vec![Complex::new(0.0_f32, 0.0); N_FFT];
    for t in 0..out_frames {
        let start = t * HOP;
        for (n, b) in buf.iter_mut().enumerate() {
            *b = Complex::new(padded[start + n] * window[n], 0.0);
        }
        fft.process(&mut buf);
        let mut power = [0.0_f32; N_FREQ];
        for (k, p) in power.iter_mut().enumerate() {
            *p = buf[k].re * buf[k].re + buf[k].im * buf[k].im;
        }
        for (m, filt) in bank.iter().enumerate() {
            let mut acc = 0.0_f32;
            for k in 0..N_FREQ {
                acc += filt[k] * power[k];
            }
            let log = acc.max(1e-10).log10().max(min_val);
            mel[m * out_frames + t] = (log + 4.0) / 4.0;
        }
    }
    (mel, out_frames)
}

/// The mel front-end output: the `[128, frames]` log-mel buffer (row-major) and
/// its frame count, ready to hand to the encoder's conv stem.
pub struct Mel {
    /// Row-major `[num_mel_bins, frames]` log-mel values.
    pub data: Vec<f32>,
    /// Number of time frames.
    pub frames: usize,
}

/// Produces the log-mel front-end for offline transcription: silence-pad,
/// log-mel, then the `_prepare_mel` even-frame trim (drop the first column if the
/// frame count is odd). `n_left`/`n_right` are the prompt pad sizes in tokens.
pub fn prepare_mel(samples: &[f32], cfg: &AudioConfig, n_left: usize, n_right: usize) -> Mel {
    let padded = pad_audio(samples, n_left, n_right);
    let (mel, frames) = log_mel(&padded, cfg.global_log_mel_max);
    if frames % 2 == 0 {
        Mel { data: mel, frames }
    } else {
        // Drop the first time column to keep an even frame count.
        let new_frames = frames - 1;
        let mut trimmed = vec![0.0_f32; N_MELS * new_frames];
        for m in 0..N_MELS {
            for t in 0..new_frames {
                trimmed[m * new_frames + t] = mel[m * frames + (t + 1)];
            }
        }
        Mel {
            data: trimmed,
            frames: new_frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slaney_mel_roundtrips_and_bank_is_normalized() {
        for &hz in &[0.0, 250.0, 1_000.0, 4_000.0, 8_000.0] {
            let back = mel_to_hz(hz_to_mel(hz));
            assert!((back - hz).abs() < 1e-2, "roundtrip {hz} -> {back}");
        }
        let bank = mel_filter_bank();
        assert_eq!(bank.len(), N_MELS);
        // Every filter has some positive weight and none are negative.
        for (m, row) in bank.iter().enumerate() {
            assert!(row.iter().any(|&w| w > 0.0), "filter {m} all-zero");
            assert!(row.iter().all(|&w| w >= 0.0), "filter {m} has negative");
        }
    }

    #[test]
    fn pad_audio_aligns_to_token_boundary() {
        // len 100 → align to 1280, + n_right tokens, + n_left tokens.
        let padded = pad_audio(&[0.5; 100], 2, 3);
        assert_eq!(padded.len() % RAW_AUDIO_PER_TOK, 0);
        assert_eq!(padded.len(), 2 * 1280 + 1280 + 3 * 1280);
    }

    #[test]
    fn prepare_mel_yields_128_bins_and_even_frames() {
        let cfg = super::super::config::VoxtralMlxConfig::voxtral_realtime_mini_4b().audio;
        let mel = prepare_mel(&[0.1; 16_000], &cfg, 32, 17);
        assert_eq!(mel.data.len(), N_MELS * mel.frames);
        assert_eq!(mel.frames % 2, 0, "frame count must be even");
    }
}
