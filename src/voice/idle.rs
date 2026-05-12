//! Idle (silence) detection and trailing-silence trimming.
//!
//! Operates on the post-resample 16 kHz mono stream. Uses RMS over fixed
//! windows; classifies each window as voiced or silent at a -40 dBFS
//! threshold and tracks how many consecutive silent windows have arrived.
//! The capture loop terminates when that streak reaches the
//! `idle_after_secs` budget. A trim pass then drops the trailing silence
//! before the WAV header is finalised — Whisper hallucinates badly on
//! silence at the end of an input clip.

use super::wav::TARGET_SAMPLE_RATE;

/// Length of one RMS window. 100 ms at 16 kHz = 1600 samples.
pub const WINDOW_SAMPLES: usize = 1600;

/// `f32` amplitude corresponding to -40 dBFS, used as the silent/voiced
/// threshold. `10^(-40/20) = 0.01`.
pub const SILENCE_THRESHOLD_RMS: f32 = 0.01;

/// Classification of a single RMS window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClass {
    /// Window RMS at or below the silence threshold.
    Silent,
    /// Window RMS above the silence threshold.
    Voiced,
}

/// Streaming idle detector over 16 kHz mono samples.
///
/// Feed samples via [`IdleDetector::push`]; it buffers partial windows
/// internally and only classifies a window once it is fully filled.
/// [`IdleDetector::is_idle`] returns true once enough consecutive silent
/// windows have accumulated. The detector also remembers whether *any*
/// voiced window has ever been seen — the orchestrator uses that flag to
/// fail loudly on all-silence recordings (muted mic, etc.) rather than
/// emit a near-empty WAV that downstream Whisper would crash on.
pub struct IdleDetector {
    idle_after_secs: u32,
    pending: Vec<f32>,
    consecutive_silent: u32,
    any_voiced: bool,
}

impl IdleDetector {
    /// Builds a detector that fires after `idle_after_secs` seconds of
    /// uninterrupted silence on the 16 kHz stream.
    #[must_use]
    pub fn new(idle_after_secs: u32) -> Self {
        Self {
            idle_after_secs,
            pending: Vec::with_capacity(WINDOW_SAMPLES * 2),
            consecutive_silent: 0,
            any_voiced: false,
        }
    }

    /// Returns the configured idle-after threshold in seconds.
    #[must_use]
    pub fn idle_after_secs(&self) -> u32 {
        self.idle_after_secs
    }

    /// Feeds new samples; processes whatever windows are complete and
    /// updates internal state. Returns the classifications of windows
    /// completed by this call (oldest first) so callers can log or
    /// instrument them.
    pub fn push(&mut self, samples: &[f32]) -> Vec<WindowClass> {
        self.pending.extend_from_slice(samples);
        let mut classifications = Vec::new();
        while self.pending.len() >= WINDOW_SAMPLES {
            let class = classify_window(&self.pending[..WINDOW_SAMPLES]);
            classifications.push(class);
            match class {
                WindowClass::Silent => self.consecutive_silent += 1,
                WindowClass::Voiced => {
                    self.consecutive_silent = 0;
                    self.any_voiced = true;
                }
            }
            self.pending.drain(..WINDOW_SAMPLES);
        }
        classifications
    }

    /// Returns true once `idle_after_secs` consecutive silent windows have
    /// arrived. (At 100 ms per window, that's `10 * idle_after_secs`
    /// windows.) Always false before `idle_after_secs == 0` — a zero
    /// threshold disables auto-stop.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        if self.idle_after_secs == 0 {
            return false;
        }
        let needed = u64::from(self.idle_after_secs) * windows_per_second_u64();
        u64::from(self.consecutive_silent) >= needed
    }

    /// Returns true if at least one voiced window has been observed since
    /// construction.
    #[must_use]
    pub fn has_any_voice(&self) -> bool {
        self.any_voiced
    }

    /// Number of samples to drop from the tail of the stream when the
    /// detector has fired — exactly the silent window streak that caused
    /// `is_idle` to flip true.
    #[must_use]
    pub fn trailing_silence_samples(&self) -> usize {
        self.consecutive_silent as usize * WINDOW_SAMPLES
    }
}

fn classify_window(window: &[f32]) -> WindowClass {
    if rms(window) > SILENCE_THRESHOLD_RMS {
        WindowClass::Voiced
    } else {
        WindowClass::Silent
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

const fn windows_per_second_u64() -> u64 {
    TARGET_SAMPLE_RATE as u64 / WINDOW_SAMPLES as u64
}

/// Trims `tail_samples` from the end of `samples`, never below zero length.
///
/// Returns a borrowed slice; callers that need an owned `Vec` should
/// `.to_vec()` afterwards. If `tail_samples` exceeds the buffer length,
/// the result is the empty slice — the orchestrator catches that case
/// upstream by checking [`IdleDetector::has_any_voice`].
#[must_use]
pub fn trim_trailing_silence(samples: &[f32], tail_samples: usize) -> &[f32] {
    let end = samples.len().saturating_sub(tail_samples);
    &samples[..end]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn silence_only_input_fires_at_exact_window_budget() {
        let mut det = IdleDetector::new(2); // 2 s ⇒ 20 windows
        let one_window = vec![0.0_f32; WINDOW_SAMPLES];
        // 19 silent windows: not yet idle.
        for _ in 0..19 {
            det.push(&one_window);
        }
        assert!(!det.is_idle(), "should not be idle at 19 silent windows");
        det.push(&one_window);
        assert!(det.is_idle(), "should be idle at 20 silent windows");
        assert!(
            !det.has_any_voice(),
            "no voiced window should have been seen"
        );
        assert_eq!(det.trailing_silence_samples(), 20 * WINDOW_SAMPLES);
    }

    #[test]
    fn voiced_window_resets_silent_streak() {
        let mut det = IdleDetector::new(1); // 1 s ⇒ 10 windows
        let silent = vec![0.0_f32; WINDOW_SAMPLES];
        // Loud window: amplitude 0.5, RMS = 0.5 ≫ threshold.
        let loud = vec![0.5_f32; WINDOW_SAMPLES];
        for _ in 0..9 {
            det.push(&silent);
        }
        assert!(!det.is_idle());
        det.push(&loud); // resets
        assert!(det.has_any_voice());
        for _ in 0..9 {
            det.push(&silent);
        }
        assert!(!det.is_idle(), "9 < 10 silent windows after the reset");
        det.push(&silent);
        assert!(det.is_idle(), "10 silent windows after the reset");
    }

    #[test]
    fn voiced_only_input_never_goes_idle() {
        let mut det = IdleDetector::new(1);
        let loud = vec![0.5_f32; WINDOW_SAMPLES];
        for _ in 0..50 {
            det.push(&loud);
        }
        assert!(!det.is_idle());
        assert!(det.has_any_voice());
        assert_eq!(det.trailing_silence_samples(), 0);
    }

    #[test]
    fn partial_window_is_buffered_until_full() {
        let mut det = IdleDetector::new(1);
        let half = vec![0.0_f32; WINDOW_SAMPLES / 2];
        // Two half-windows complete one window.
        let c1 = det.push(&half);
        assert!(c1.is_empty(), "first half does not complete a window");
        let c2 = det.push(&half);
        assert_eq!(c2, vec![WindowClass::Silent]);
    }

    #[test]
    fn rms_boundary_classification() {
        // Sample value v with RMS = v (constant signal). 0.0099 < threshold, 0.0101 > threshold.
        let below = vec![0.0099_f32; WINDOW_SAMPLES];
        let above = vec![0.0101_f32; WINDOW_SAMPLES];
        assert_eq!(classify_window(&below), WindowClass::Silent);
        assert_eq!(classify_window(&above), WindowClass::Voiced);
    }

    #[test]
    fn idle_after_zero_disables_autostop() {
        let mut det = IdleDetector::new(0);
        let silent = vec![0.0_f32; WINDOW_SAMPLES];
        for _ in 0..100 {
            det.push(&silent);
        }
        assert!(
            !det.is_idle(),
            "idle_after_secs=0 should never trigger auto-stop"
        );
    }

    #[test]
    fn trim_trailing_silence_drops_exactly_the_tail() {
        let samples: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let trimmed = trim_trailing_silence(&samples, 300);
        assert_eq!(trimmed.len(), 700);
        // Float comparisons are safe here: every sample is `i as f32`
        // for small integers, which round-trips exactly.
        assert!((trimmed[0] - 0.0).abs() < f32::EPSILON);
        assert!((trimmed[1] - 1.0).abs() < f32::EPSILON);
        assert!((trimmed[2] - 2.0).abs() < f32::EPSILON);
        assert!((trimmed[699] - 699.0).abs() < f32::EPSILON);
    }

    #[test]
    fn trim_trailing_silence_clamps_to_empty_on_overshoot() {
        let samples = vec![1.0, 2.0, 3.0];
        let trimmed = trim_trailing_silence(&samples, 99);
        assert!(trimmed.is_empty());
    }
}
