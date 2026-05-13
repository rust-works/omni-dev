//! End-to-end capture pipeline orchestrator.
//!
//! Glues an [`AudioSource`] through the write-path stages (mixdown →
//! resample → idle detection → trailing-silence trim → WAV write) and
//! reports a structured [`CaptureSummary`] when done. Signal-driven
//! termination is wired up via [`install_ctrl_c_handler`], which the CLI
//! entry point calls before delegating to [`run_capture`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use super::audio::AudioSource;
use super::idle::{trim_trailing_silence, IdleDetector};
use super::wav::{mono_mixdown, Resampler, WavWriter};

/// Options for a single capture session.
#[derive(Debug, Clone)]
pub struct CaptureOpts {
    /// Destination WAV path.
    pub output: PathBuf,
    /// Seconds of trailing silence that auto-stop capture. `0` disables
    /// auto-stop (capture runs until the source is exhausted or a stop
    /// signal arrives).
    pub idle_after_secs: u32,
}

impl CaptureOpts {
    /// Creates a new options struct with the given output path and
    /// idle-after threshold.
    #[must_use]
    pub fn new(output: impl Into<PathBuf>, idle_after_secs: u32) -> Self {
        Self {
            output: output.into(),
            idle_after_secs,
        }
    }
}

/// Why capture stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationReason {
    /// The idle (silence) detector fired its `idle_after_secs` budget.
    Idle,
    /// The [`AudioSource`] returned `None` (file end, stream closed).
    SourceExhausted,
    /// An external stop signal flipped the supplied `AtomicBool` to true
    /// (Ctrl-C in production).
    Signal,
}

/// Structured summary of a capture session for tests and logging.
#[derive(Debug, Clone)]
pub struct CaptureSummary {
    /// Path the WAV was written to.
    pub output: PathBuf,
    /// Samples actually written to disk (post-trim).
    pub samples_written: u64,
    /// Samples that were dropped from the tail by trailing-silence trim.
    /// Always 0 for [`TerminationReason::Signal`] (user-driven stops are
    /// not trimmed — the user chose where to stop).
    pub trimmed_samples: u64,
    /// Why capture stopped.
    pub terminated_by: TerminationReason,
}

/// Drives `source` through the pipeline and writes a 16 kHz mono 16-bit
/// PCM WAV at `opts.output`.
///
/// Termination conditions, in priority order each loop iteration:
///
/// 1. `stop_signal` set to true → [`TerminationReason::Signal`].
/// 2. Idle detector fires → [`TerminationReason::Idle`].
/// 3. Source returns `None` → [`TerminationReason::SourceExhausted`].
///
/// On [`TerminationReason::Idle`] the trailing silence that caused the
/// trigger is trimmed before the WAV header is finalised. On
/// [`TerminationReason::Signal`] nothing is trimmed (the user chose the
/// cutoff). On [`TerminationReason::SourceExhausted`] the entire
/// resampled stream is written verbatim.
///
/// Returns an error if no voiced window was ever observed — emitting a
/// near-empty WAV would just crash downstream Whisper anyway, so we
/// fail loudly instead. The output file is removed on this path so the
/// caller never sees a malformed WAV on disk.
pub fn run_capture<S: AudioSource>(
    mut source: S,
    opts: CaptureOpts,
    stop_signal: Arc<AtomicBool>,
) -> Result<CaptureSummary> {
    let mut resampler = Resampler::new(source.sample_rate())
        .with_context(|| format!("Failed to build resampler at {} Hz", source.sample_rate()))?;
    let mut detector = IdleDetector::new(opts.idle_after_secs);
    let mut buffer: Vec<f32> = Vec::new();
    let channels = source.channels();
    let termination = loop {
        if stop_signal.load(Ordering::Relaxed) {
            break TerminationReason::Signal;
        }
        let Some(chunk) = source.next_chunk() else {
            break TerminationReason::SourceExhausted;
        };
        let mono = mono_mixdown(&chunk, channels);
        let resampled = resampler.push(&mono)?;
        detector.push(&resampled);
        buffer.extend_from_slice(&resampled);
        if detector.is_idle() {
            break TerminationReason::Idle;
        }
    };

    // Drain the resampler tail when the source exhausted naturally. We
    // skip this on Signal-driven shutdown to keep the cut crisp.
    if matches!(
        termination,
        TerminationReason::SourceExhausted | TerminationReason::Idle
    ) {
        let tail = resampler.flush()?;
        detector.push(&tail);
        buffer.extend_from_slice(&tail);
    }

    // Trim trailing silence only when the idle detector fired.
    let (samples_to_write, trimmed): (&[f32], u64) = if termination == TerminationReason::Idle {
        let tail = detector.trailing_silence_samples();
        let trimmed = trim_trailing_silence(&buffer, tail);
        let dropped = (buffer.len() - trimmed.len()) as u64;
        (trimmed, dropped)
    } else {
        (buffer.as_slice(), 0)
    };

    // No-audio guard: on natural termination (idle / source exhausted) we
    // require at least one voiced window. On signal-driven termination the
    // user explicitly stopped, so we instead just require *some* samples
    // to write — Ctrl-C before anything was captured is also a hard error,
    // but a quiet recording the user chose to keep is not.
    match termination {
        TerminationReason::Idle | TerminationReason::SourceExhausted => {
            if !detector.has_any_voice() {
                return Err(anyhow!(
                    "No audio detected — every window of the {:.1}s capture was below the \
                     silence threshold. Is the microphone muted or routed to a different device?",
                    elapsed_seconds(buffer.len())
                ));
            }
        }
        TerminationReason::Signal => {
            if samples_to_write.is_empty() {
                return Err(anyhow!(
                    "Stopped before any audio was captured — the stop signal arrived before \
                     the first sample reached the writer."
                ));
            }
        }
    }

    let mut writer = WavWriter::create(&opts.output)?;
    writer.write_samples(samples_to_write)?;
    let samples_written = writer.samples_written();
    writer.finalize()?;

    Ok(CaptureSummary {
        output: opts.output,
        samples_written,
        trimmed_samples: trimmed,
        terminated_by: termination,
    })
}

fn elapsed_seconds(samples_at_16k: usize) -> f32 {
    samples_at_16k as f32 / super::wav::TARGET_SAMPLE_RATE as f32
}

/// Installs a SIGINT (Ctrl-C) handler that flips the returned flag on
/// receipt, and returns a fresh `Arc<AtomicBool>` initialised to false.
///
/// The flag is the one [`run_capture`] polls each iteration; flipping it
/// causes the pipeline to terminate with [`TerminationReason::Signal`].
/// On Unix this uses `signal-hook` (safe, no global state hijack). On
/// Windows the same `signal-hook` API targets `SIGINT` via the
/// console-control handler; the call is portable.
///
/// Safe to call once per process. A second call adds a *second* handler
/// that also flips the flag — harmless but redundant. The capture loop
/// already terminates on the first flip.
pub fn install_ctrl_c_handler() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone())
        .context("Failed to register SIGINT handler")?;
    Ok(flag)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::f32::consts::TAU;

    use crate::voice::audio::FileAudioSource;

    fn voiced_then_silent(rate: u32, voiced_s: f32, silent_s: f32, amplitude: f32) -> Vec<f32> {
        let voiced_n = (rate as f32 * voiced_s) as usize;
        let silent_n = (rate as f32 * silent_s) as usize;
        let mut out: Vec<f32> = (0..voiced_n)
            .map(|i| amplitude * (TAU * 440.0 * i as f32 / rate as f32).sin())
            .collect();
        out.extend(std::iter::repeat_n(0.0, silent_n));
        out
    }

    fn write_to_temp(prefix: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(format!("{prefix}.wav"));
        (tmp, path)
    }

    fn stop_flag(value: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(value))
    }

    #[test]
    fn idle_termination_trims_trailing_silence() -> Result<()> {
        // 1 s voiced @ 0.4 amp, then 3 s silence. idle_after_secs=2 → fires
        // after 2 s of silence; one extra second of silence remains in the
        // buffer that should be trimmed.
        let source_rate = 48_000;
        let samples = voiced_then_silent(source_rate, 1.0, 3.0, 0.4);
        let source = FileAudioSource::from_samples(samples, source_rate, 1, 4800);
        let (_tmp, path) = write_to_temp("idle");
        let summary = run_capture(source, CaptureOpts::new(&path, 2), stop_flag(false))?;

        assert_eq!(summary.terminated_by, TerminationReason::Idle);
        assert!(
            summary.trimmed_samples > 0,
            "tail silence should be trimmed"
        );

        let reader = hound::WavReader::open(&path)?;
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);

        let frame_count = reader.duration() as usize;
        // Expected ≈ 1 s of voiced content at 16 kHz (~16_000 samples).
        // Sinc warm-up and window-alignment slack mean ±2_000 frames is
        // routine. The hard contract is that *some* of the 3 s of silence
        // was trimmed.
        assert!(
            (14_000..=20_000).contains(&frame_count),
            "unexpected frame count after trim: {frame_count}"
        );
        Ok(())
    }

    #[test]
    fn source_exhausted_writes_everything() -> Result<()> {
        // 0.5 s voiced @ 0.4 amp. idle_after_secs huge → never fires; source
        // runs out naturally.
        let source_rate = 16_000; // identity path through resampler
        let voiced_n = 8000;
        let samples: Vec<f32> = (0..voiced_n)
            .map(|i| 0.4 * (TAU * 440.0 * i as f32 / source_rate as f32).sin())
            .collect();
        let source = FileAudioSource::from_samples(samples, source_rate, 1, 1024);
        let (_tmp, path) = write_to_temp("exhausted");
        let summary = run_capture(source, CaptureOpts::new(&path, 60), stop_flag(false))?;

        assert_eq!(summary.terminated_by, TerminationReason::SourceExhausted);
        assert_eq!(summary.trimmed_samples, 0);
        assert_eq!(summary.samples_written, voiced_n as u64);
        Ok(())
    }

    /// Test-only [`AudioSource`] that wraps another source and flips the
    /// supplied stop signal after `flip_after_chunks` calls. Lets us
    /// exercise the signal-termination path deterministically without
    /// race conditions.
    struct SignalFlippingSource<S: AudioSource> {
        inner: S,
        stop: Arc<AtomicBool>,
        chunks_returned: u32,
        flip_after_chunks: u32,
    }

    impl<S: AudioSource> AudioSource for SignalFlippingSource<S> {
        fn next_chunk(&mut self) -> Option<Vec<f32>> {
            let chunk = self.inner.next_chunk();
            if chunk.is_some() {
                self.chunks_returned += 1;
                if self.chunks_returned >= self.flip_after_chunks {
                    self.stop.store(true, Ordering::Relaxed);
                }
            }
            chunk
        }
        fn sample_rate(&self) -> u32 {
            self.inner.sample_rate()
        }
        fn channels(&self) -> u16 {
            self.inner.channels()
        }
    }

    #[test]
    fn signal_termination_does_not_trim() -> Result<()> {
        // Long voiced source; signal flips after the first chunk reaches
        // the loop body, so some voiced audio always lands in the buffer.
        let source_rate = 16_000;
        let samples: Vec<f32> = (0..160_000)
            .map(|i| 0.4 * (TAU * 440.0 * i as f32 / source_rate as f32).sin())
            .collect();
        let inner = FileAudioSource::from_samples(samples, source_rate, 1, 4000);
        let stop = stop_flag(false);
        let source = SignalFlippingSource {
            inner,
            stop: stop.clone(),
            chunks_returned: 0,
            flip_after_chunks: 1,
        };
        let (_tmp, path) = write_to_temp("signal");
        let summary = run_capture(source, CaptureOpts::new(&path, 5), stop)?;

        assert_eq!(summary.terminated_by, TerminationReason::Signal);
        assert_eq!(
            summary.trimmed_samples, 0,
            "signal termination must not trim"
        );
        assert!(
            summary.samples_written > 0,
            "should have captured something"
        );
        // File exists and is readable.
        let reader = hound::WavReader::open(&path)?;
        assert_eq!(reader.spec().sample_rate, 16_000);
        Ok(())
    }

    #[test]
    fn signal_termination_with_no_captured_audio_fails_loudly() {
        // Pre-flip the signal: the loop's top-of-iteration check exits
        // before reading anything. The orchestrator must error rather
        // than write a zero-sample WAV.
        let source_rate = 16_000;
        let source = FileAudioSource::from_samples(vec![0.0; 16_000], source_rate, 1, 16);
        let stop = stop_flag(true);
        let (_tmp, path) = write_to_temp("signal_empty");
        let err = run_capture(source, CaptureOpts::new(&path, 5), stop).unwrap_err();
        assert!(
            err.to_string()
                .contains("Stopped before any audio was captured"),
            "expected loud failure, got: {err}"
        );
    }

    #[test]
    fn silence_only_input_fails_loudly() {
        let source_rate = 16_000;
        let silence = vec![0.0_f32; 16_000 * 6]; // 6 s of silence
        let source = FileAudioSource::from_samples(silence, source_rate, 1, 1024);
        let (_tmp, path) = write_to_temp("silent");
        let err = run_capture(source, CaptureOpts::new(&path, 2), stop_flag(false)).unwrap_err();
        assert!(
            err.to_string().contains("No audio detected"),
            "expected loud failure, got: {err}"
        );
        // The output file may have been created and then is overwritten by
        // a later run, but it should not have been finalised — assert the
        // error path didn't accidentally succeed and return a summary.
    }
}
