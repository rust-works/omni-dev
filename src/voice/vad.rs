//! Pure-Rust voice-activity gate — the silence-boundary authority for the
//! streaming transcriber (#974), ported from the #969 spike.
//!
//! ## Why earshot, not Silero-via-candle-onnx
//!
//! The #969 spike first tried running the Silero VAD v5 ONNX graph through
//! pure-Rust `candle-onnx`. candle-onnx 0.10 *parses and executes* the graph
//! end-to-end (its sample-rate `If` subgraph, STFT-as-`Conv`, and `LSTM`)
//! without error — but the output is **numerically wrong**: a near-constant
//! ~0.0005 for silence, a loud 200 Hz sine, and real speech alike (expected
//! range 0…0.95). Op *presence* in candle-onnx's match table did not
//! guarantee correct *execution*. So Silero is not viable on the pure-Rust
//! stack today and we use [`earshot`] — a pure-Rust WebRTC-style GMM VAD with
//! **zero C++/ONNX/`ort`**, preserving the "zero native toolchain"
//! cross-platform gate. See ADR-0040.
//!
//! ## Re-framing
//!
//! Callers push arbitrary-length chunks (the streaming transcriber pumps
//! 100 ms / 1600-sample chunks); earshot scores fixed **256-sample (16 ms)
//! windows at 16 kHz**. `VadGate` re-frames the stream into 256-sample
//! windows using the same pending-buffer drain as
//! [`crate::voice::idle::IdleDetector`] so no audio is dropped at chunk
//! boundaries.

use earshot::{DefaultPredictor, Detector};

use crate::voice::wav::TARGET_SAMPLE_RATE;

/// earshot fixed analysis window: 256 samples = 16 ms at 16 kHz.
pub const VAD_WINDOW: usize = 256;

/// VAD analysis windows per second at 16 kHz (16000 / 256 = 62.5).
fn windows_per_second() -> f32 {
    TARGET_SAMPLE_RATE as f32 / VAD_WINDOW as f32
}

/// Voice-activity gate re-framing pushed chunks into earshot's 256-sample
/// windows. Acts as the endpoint authority: tracks consecutive silent
/// windows and reports `is_idle()` once silence persists.
pub struct VadGate {
    detector: Box<Detector<DefaultPredictor>>,
    pending: Vec<f32>,
    threshold: f32,
    consecutive_silent: u32,
    silence_windows_needed: u32,
    any_voiced: bool,
}

impl VadGate {
    /// Builds a gate. `threshold`: speech-score cut in `[0,1]`
    /// ("aggressiveness"; earshot's own guidance is ~0.5, lower = more
    /// permissive). `silence_secs`: consecutive silence (may be fractional)
    /// before `is_idle()` trips.
    #[must_use]
    pub fn new(threshold: f32, silence_secs: f32) -> Self {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // window count is tiny and non-negative
        let silence_windows_needed = (silence_secs * windows_per_second()).round() as u32;
        Self {
            detector: Detector::default_boxed(),
            pending: Vec::with_capacity(VAD_WINDOW * 8),
            threshold,
            consecutive_silent: 0,
            silence_windows_needed,
            any_voiced: false,
        }
    }

    /// Feeds raw f32 samples (any length, expected in `[-1, 1]`). Drains
    /// complete 256-sample windows, classifies each as voiced
    /// (`score >= threshold`), updates endpoint counters, and returns
    /// `(voiced, score)` per consumed window. Leftover (< 256) stays in
    /// `pending` — no audio is lost.
    pub fn push(&mut self, samples: &[f32]) -> Vec<(bool, f32)> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= VAD_WINDOW {
            let frame: Vec<f32> = self.pending[..VAD_WINDOW].to_vec();
            self.pending.drain(..VAD_WINDOW);
            let score = self.detector.predict_f32(&frame);
            let voiced = score >= self.threshold;
            if voiced {
                self.consecutive_silent = 0;
                self.any_voiced = true;
            } else {
                self.consecutive_silent += 1;
            }
            out.push((voiced, score));
        }
        out
    }

    /// Returns true once consecutive silent windows reach the
    /// `silence_secs` budget. `silence_secs == 0` disables auto-endpointing
    /// (mirrors [`crate::voice::idle::IdleDetector`]).
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.silence_windows_needed > 0 && self.consecutive_silent >= self.silence_windows_needed
    }

    /// Returns whether any voiced window has been seen since the last
    /// `reset()` — lets callers fail loudly on all-silence input (muted
    /// mic / wrong fixture).
    #[must_use]
    pub fn has_any_voice(&self) -> bool {
        self.any_voiced
    }

    /// Resets endpoint state for a new utterance after a confirmed silence
    /// boundary: resets the detector's internal feature history, drops the
    /// (silent) pending tail, and clears the silence counters.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.pending.clear();
        self.consecutive_silent = 0;
        self.any_voiced = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1600-sample chunks must re-frame into 256-sample windows with no sample
    /// loss: each call yields ⌊pending/256⌋ windows, remainder carried.
    #[test]
    fn reframing_consumes_256_windows_without_sample_loss() {
        let mut gate = VadGate::new(0.5, 2.0);
        let chunk = vec![0.0f32; 1600];

        // 1600 -> 6 windows (1536), 64 retained.
        let w1 = gate.push(&chunk);
        assert_eq!(w1.len(), 6);
        assert_eq!(gate.pending.len(), 64);

        // 64 + 1600 = 1664 -> 6 windows (1536), 128 retained.
        let w2 = gate.push(&chunk);
        assert_eq!(w2.len(), 6);
        assert_eq!(gate.pending.len(), 128);

        // 128 + 1600 = 1728 -> 6 windows (1536), 192 retained.
        let w3 = gate.push(&chunk);
        assert_eq!(w3.len(), 6);
        assert_eq!(gate.pending.len(), 192);

        // No audio lost: windows_consumed * 256 + pending == total pushed.
        let total_windows = w1.len() + w2.len() + w3.len();
        assert_eq!(total_windows * VAD_WINDOW + gate.pending.len(), 3 * 1600);
    }

    /// All-silence input must never register voice and must eventually go idle.
    #[test]
    fn silence_goes_idle_and_reports_no_voice() {
        let mut gate = VadGate::new(0.5, 1.0);
        // 1 s of silence is ~62.5 windows > needed (~63); push 2 s to be safe.
        for _ in 0..20 {
            gate.push(&vec![0.0f32; 1600]);
        }
        assert!(
            !gate.has_any_voice(),
            "pure silence must not register voice"
        );
        assert!(gate.is_idle(), "1 s of silence should trip the endpoint");
    }

    /// `silence_secs == 0` disables auto-endpointing entirely.
    #[test]
    fn zero_silence_secs_never_idles() {
        let mut gate = VadGate::new(0.5, 0.0);
        for _ in 0..20 {
            gate.push(&vec![0.0f32; 1600]);
        }
        assert!(!gate.is_idle(), "silence_secs=0 must disable the endpoint");
    }

    /// `reset()` clears the pending tail, silence counters, and voice flag.
    #[test]
    fn reset_clears_pending_and_counters() {
        let mut gate = VadGate::new(0.5, 0.5);
        gate.push(&vec![0.0f32; 1700]); // leaves a pending remainder
        assert!(!gate.pending.is_empty());
        for _ in 0..10 {
            gate.push(&vec![0.0f32; 1600]);
        }
        assert!(gate.is_idle());
        gate.reset();
        assert!(gate.pending.is_empty());
        assert!(!gate.is_idle());
        assert!(!gate.has_any_voice());
    }
}
