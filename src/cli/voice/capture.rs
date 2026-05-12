//! `omni-dev voice capture` — record microphone audio to a 16 kHz mono WAV file.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;

use crate::voice::capture::{
    install_ctrl_c_handler, run_capture, CaptureOpts, CaptureSummary, TerminationReason,
};
use crate::voice::CpalAudioSource;

/// Default idle-after threshold in seconds. Matches the issue spec.
pub const DEFAULT_IDLE_AFTER_SECS: u32 = 5;

/// Captures audio from a microphone to a 16 kHz mono WAV file.
///
/// Auto-stops after `--idle-after` seconds of trailing silence (default 5 s)
/// or when Ctrl-C is pressed. The output WAV is 16 kHz mono 16-bit signed
/// PCM (whisper.cpp convention).
#[derive(Parser)]
pub struct CaptureCommand {
    /// Stop after this many seconds of trailing silence. `0` disables
    /// auto-stop — capture runs until Ctrl-C.
    #[arg(long, default_value_t = DEFAULT_IDLE_AFTER_SECS)]
    pub idle_after: u32,

    /// Destination WAV path. Defaults to
    /// `~/.omni-dev/voice/captures/<UTC-timestamp>.wav`.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Audio input device name. Defaults to the system default input.
    /// Matching is exact against the platform-reported device name; an
    /// unknown name errors with a list of detected devices.
    #[arg(long)]
    pub device: Option<String>,
}

impl CaptureCommand {
    /// Executes the capture command.
    pub fn execute(self) -> Result<()> {
        let output = match self.output {
            Some(path) => path,
            None => default_output_path()?,
        };
        let opts = CaptureOpts::new(output, self.idle_after);
        let stop = install_ctrl_c_handler()?;
        let source = CpalAudioSource::new(self.device.as_deref())?;

        eprintln!(
            "Recording to {} (idle-after: {}s, Ctrl-C to stop)…",
            opts.output.display(),
            opts.idle_after_secs
        );
        let summary = run_capture(source, opts, stop)?;
        print_summary(&summary);
        Ok(())
    }
}

/// Resolves the default output path used when `--output` is not supplied:
/// `~/.omni-dev/voice/captures/<YYYYMMDDTHHMMSSZ>.wav`.
fn default_output_path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .context("Failed to resolve the user's home directory for default --output path")?;
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    Ok(home
        .join(".omni-dev")
        .join("voice")
        .join("captures")
        .join(format!("{timestamp}.wav")))
}

fn print_summary(summary: &CaptureSummary) {
    let reason = match summary.terminated_by {
        TerminationReason::Idle => "silence threshold reached",
        TerminationReason::SourceExhausted => "audio source ended",
        TerminationReason::Signal => "Ctrl-C",
    };
    let seconds = samples_to_seconds(summary.samples_written);
    eprintln!(
        "Captured {seconds:.2}s ({} samples; {} trimmed; stopped: {reason}) → {}",
        summary.samples_written,
        summary.trimmed_samples,
        summary.output.display()
    );
}

fn samples_to_seconds(samples: u64) -> f64 {
    samples as f64 / f64::from(crate::voice::wav::TARGET_SAMPLE_RATE)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        capture: CaptureCommand,
    }

    #[test]
    fn parses_defaults() {
        let cli = TestCli::try_parse_from(["test"]).unwrap();
        assert_eq!(cli.capture.idle_after, DEFAULT_IDLE_AFTER_SECS);
        assert!(cli.capture.output.is_none());
        assert!(cli.capture.device.is_none());
    }

    #[test]
    fn parses_all_flags() {
        let cli = TestCli::try_parse_from([
            "test",
            "--idle-after",
            "10",
            "--output",
            "/tmp/x.wav",
            "--device",
            "MacBook Pro Microphone",
        ])
        .unwrap();
        assert_eq!(cli.capture.idle_after, 10);
        assert_eq!(
            cli.capture.output.as_deref().map(|p| p.to_str().unwrap()),
            Some("/tmp/x.wav")
        );
        assert_eq!(
            cli.capture.device.as_deref(),
            Some("MacBook Pro Microphone")
        );
    }

    #[test]
    fn parses_idle_after_zero() {
        let cli = TestCli::try_parse_from(["test", "--idle-after", "0"]).unwrap();
        assert_eq!(cli.capture.idle_after, 0);
    }

    #[test]
    fn rejects_negative_idle_after() {
        let result = TestCli::try_parse_from(["test", "--idle-after", "-1"]);
        assert!(result.is_err(), "negative idle-after should be rejected");
    }

    #[test]
    fn default_output_path_uses_utc_timestamp() {
        let path = default_output_path().unwrap();
        let s = path.to_string_lossy();
        assert!(s.contains(".omni-dev"));
        assert!(s.contains("voice"));
        assert!(s.contains("captures"));
        assert!(s.ends_with(".wav"));
    }
}
