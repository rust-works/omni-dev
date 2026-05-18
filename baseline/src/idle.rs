//! Silence-gap RMS endpoint detector — copied verbatim from
//! `src/voice/idle.rs` to keep the spike builds self-contained.
//! Both candidates use this for fairness: don't bring in either
//! runtime's bundled VAD.

pub const SAMPLE_RATE: usize = 16_000;
pub const WINDOW_SAMPLES: usize = 1600;
pub const SILENCE_THRESHOLD_RMS: f32 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClass {
    Silent,
    Voiced,
}

pub struct IdleDetector {
    idle_after_secs: u32,
    pending: Vec<f32>,
    consecutive_silent: u32,
    any_voiced: bool,
}

impl IdleDetector {
    #[must_use]
    pub fn new(idle_after_secs: u32) -> Self {
        Self {
            idle_after_secs,
            pending: Vec::with_capacity(WINDOW_SAMPLES * 2),
            consecutive_silent: 0,
            any_voiced: false,
        }
    }

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

    #[must_use]
    pub fn is_idle(&self) -> bool {
        if self.idle_after_secs == 0 {
            return false;
        }
        let needed = u64::from(self.idle_after_secs) * windows_per_second_u64();
        u64::from(self.consecutive_silent) >= needed
    }

    #[must_use]
    pub fn has_any_voice(&self) -> bool {
        self.any_voiced
    }

    #[must_use]
    pub fn consecutive_silent_windows(&self) -> u32 {
        self.consecutive_silent
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
    SAMPLE_RATE as u64 / WINDOW_SAMPLES as u64
}
