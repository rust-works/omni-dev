//! `omni-dev voice enroll` — capture a microphone sample, compute the
//! speaker embedding, and persist to `~/.omni-dev/voice/speakers/<name>.json`.
//!
//! Stops on the first of: `--idle-after` seconds of trailing silence,
//! `--max-secs` elapsed since start, or Ctrl-C. Refuses to overwrite an
//! existing enrolment unless `--force` is set.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Parser;

use crate::voice::capture::{
    install_ctrl_c_handler, run_capture, CaptureOpts, CaptureSummary, TerminationReason,
};
use crate::voice::models::SPEAKER_WESPEAKER_EN;
use crate::voice::{
    captures_dir, speaker_file, CpalAudioSource, EnrolledSpeaker, WespeakerEmbedder,
};

/// Default idle-silence threshold in seconds (issue #805 spec).
pub const DEFAULT_IDLE_AFTER_SECS: u32 = 2;

/// Default maximum capture duration in seconds (issue #805 spec — 30 s cap).
pub const DEFAULT_MAX_SECS: u32 = 30;

/// Captures audio from a microphone, computes a speaker embedding, and
/// persists it to `~/.omni-dev/voice/speakers/<name>.json`.
///
/// Stops on the first of: `--idle-after` seconds of trailing silence,
/// `--max-secs` elapsed (default 30), or Ctrl-C. Refuses to overwrite an
/// existing enrolment unless `--force` is set.
#[derive(Parser)]
pub struct EnrollCommand {
    /// Identifier under which to store the embedding (the JSON filename
    /// stem). Defaults to `default`.
    #[arg(long, default_value = "default")]
    pub name: String,

    /// Stop after this many seconds of trailing silence.
    #[arg(long, default_value_t = DEFAULT_IDLE_AFTER_SECS)]
    pub idle_after: u32,

    /// Hard upper bound on capture duration in seconds. Capture stops as
    /// soon as this many seconds have elapsed, even if speech continues.
    /// `0` disables the cap (only idle/Ctrl-C will stop the capture).
    #[arg(long, default_value_t = DEFAULT_MAX_SECS)]
    pub max_secs: u32,

    /// Audio input device name. Defaults to the system default input.
    #[arg(long)]
    pub device: Option<String>,

    /// Path to the wespeaker ONNX model. Overrides the default at
    /// `~/.omni-dev/voice/models/wespeaker-en-voxceleb-resnet34-LM/` and
    /// the `OMNI_DEV_VOICE_SPEAKER_MODEL` env var.
    #[arg(long)]
    pub speaker_model: Option<PathBuf>,

    /// Overwrite an existing `<name>.json` enrolment instead of refusing.
    #[arg(long)]
    pub force: bool,
}

impl EnrollCommand {
    /// Executes the enroll command.
    pub fn execute(self) -> Result<()> {
        let speaker_model = resolve_speaker_model_path(self.speaker_model.as_deref())?;
        let dest = speaker_file(&self.name)?;
        if dest.is_file() && !self.force {
            bail!(
                "speaker {} already enrolled at {}; pass --force to overwrite",
                self.name,
                dest.display()
            );
        }

        // Capture to a tempfile WAV inside the captures directory, named
        // with a leading dot so it's clear from `ls` that it's transient.
        let captures = captures_dir()?;
        std::fs::create_dir_all(&captures)
            .with_context(|| format!("create captures dir {}", captures.display()))?;
        let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let tmp_wav = captures.join(format!(".enroll-{timestamp}.wav"));

        let stop = install_ctrl_c_handler()?;
        // Optional max-duration cap: a background watchdog flips the same
        // stop signal Ctrl-C uses. Thread leaks at process exit, which is
        // fine for a one-shot CLI.
        if self.max_secs > 0 {
            let stop = stop.clone();
            let deadline_secs = u64::from(self.max_secs);
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(deadline_secs));
                stop.store(true, Ordering::Relaxed);
            });
        }

        let source = CpalAudioSource::new(self.device.as_deref())?;
        eprintln!(
            "Recording enrolment for {} to {} (idle-after: {}s, max: {}s, Ctrl-C to stop)…",
            self.name,
            tmp_wav.display(),
            self.idle_after,
            self.max_secs,
        );
        let opts = CaptureOpts::new(&tmp_wav, self.idle_after);
        let summary = run_capture(source, opts, stop)?;
        print_capture_summary(&summary);

        // Decode the captured WAV, embed, persist.
        let result = embed_and_save(&self.name, &speaker_model, &tmp_wav, &dest);

        // Always try to delete the tempfile — even if the embed step
        // failed, the WAV is no longer useful.
        let _ = std::fs::remove_file(&tmp_wav);

        result?;
        eprintln!(
            "Enrolled speaker {} ({} dim) -> {}",
            self.name,
            embed_dim_hint(),
            dest.display()
        );
        Ok(())
    }
}

fn embed_and_save(
    name: &str,
    speaker_model: &std::path::Path,
    tmp_wav: &std::path::Path,
    dest: &std::path::Path,
) -> Result<()> {
    let pcm = read_wav_16k_mono_i16(tmp_wav)?;
    let embedder = WespeakerEmbedder::new(speaker_model)?;
    let vector = embedder.embed(&pcm)?;
    let enrolled = EnrolledSpeaker {
        name: name.to_string(),
        model: SPEAKER_WESPEAKER_EN.variant.to_string(),
        dim: vector.len(),
        vector,
        samples_used: 1,
        enrolled_at: Utc::now(),
    };
    enrolled.save(dest)
}

fn read_wav_16k_mono_i16(path: &std::path::Path) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("open enrolment WAV at {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != 16_000 || spec.channels != 1 {
        bail!(
            "enrolment WAV at {} must be 16 kHz mono (got {} Hz, {} channels)",
            path.display(),
            spec.sample_rate,
            spec.channels
        );
    }
    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .context("decode enrolment WAV samples")?;
    Ok(samples)
}

fn resolve_speaker_model_path(override_path: Option<&std::path::Path>) -> Result<PathBuf> {
    let dir = SPEAKER_WESPEAKER_EN.resolve_dir(override_path)?;
    SPEAKER_WESPEAKER_EN.ensure_present(&dir)?;
    Ok(dir.join(SPEAKER_WESPEAKER_EN.required_files[0]))
}

fn print_capture_summary(summary: &CaptureSummary) {
    eprintln!("{}", format_capture_summary(summary));
}

fn format_capture_summary(summary: &CaptureSummary) -> String {
    let reason = match summary.terminated_by {
        TerminationReason::Idle => "silence threshold reached",
        TerminationReason::SourceExhausted => "audio source ended",
        TerminationReason::Signal => "Ctrl-C or max-secs deadline",
    };
    let seconds = samples_to_seconds(summary.samples_written);
    format!(
        "Captured {seconds:.2}s ({} samples; {} trimmed; stopped: {reason})",
        summary.samples_written, summary.trimmed_samples,
    )
}

fn samples_to_seconds(samples: u64) -> f64 {
    samples as f64 / f64::from(crate::voice::wav::TARGET_SAMPLE_RATE)
}

const fn embed_dim_hint() -> usize {
    // The actual dim comes from the model output and is verified at
    // save() time; this hint is just for the stderr summary.
    256
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        enroll: EnrollCommand,
    }

    #[test]
    fn parses_defaults() {
        let cli = TestCli::try_parse_from(["test"]).unwrap();
        assert_eq!(cli.enroll.name, "default");
        assert_eq!(cli.enroll.idle_after, DEFAULT_IDLE_AFTER_SECS);
        assert_eq!(cli.enroll.max_secs, DEFAULT_MAX_SECS);
        assert!(cli.enroll.device.is_none());
        assert!(cli.enroll.speaker_model.is_none());
        assert!(!cli.enroll.force);
    }

    #[test]
    fn parses_all_flags() {
        let cli = TestCli::try_parse_from([
            "test",
            "--name",
            "jky",
            "--idle-after",
            "3",
            "--max-secs",
            "20",
            "--device",
            "Built-in Mic",
            "--speaker-model",
            "/opt/wespeaker.onnx",
            "--force",
        ])
        .unwrap();
        assert_eq!(cli.enroll.name, "jky");
        assert_eq!(cli.enroll.idle_after, 3);
        assert_eq!(cli.enroll.max_secs, 20);
        assert_eq!(cli.enroll.device.as_deref(), Some("Built-in Mic"));
        assert_eq!(
            cli.enroll.speaker_model.as_deref().and_then(|p| p.to_str()),
            Some("/opt/wespeaker.onnx")
        );
        assert!(cli.enroll.force);
    }

    #[test]
    fn parses_max_secs_zero_disables_cap() {
        let cli = TestCli::try_parse_from(["test", "--max-secs", "0"]).unwrap();
        assert_eq!(cli.enroll.max_secs, 0);
    }

    #[test]
    fn rejects_negative_idle_after() {
        let r = TestCli::try_parse_from(["test", "--idle-after", "-1"]);
        assert!(r.is_err());
    }

    #[test]
    fn rejects_negative_max_secs() {
        let r = TestCli::try_parse_from(["test", "--max-secs", "-1"]);
        assert!(r.is_err());
    }

    #[test]
    fn resolve_speaker_model_path_errors_with_install_hint_when_dir_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let Err(err) = resolve_speaker_model_path(Some(tmp.path())) else {
            panic!("empty model dir should fail the ensure_present check");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Speaker model found"), "got: {msg}");
        assert!(msg.contains("--variant speaker-wespeaker-en"), "got: {msg}");
    }

    #[test]
    fn resolve_speaker_model_path_returns_onnx_file_when_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let onnx = tmp.path().join(SPEAKER_WESPEAKER_EN.required_files[0]);
        std::fs::write(&onnx, b"placeholder").unwrap();
        let resolved = resolve_speaker_model_path(Some(tmp.path())).unwrap();
        assert_eq!(resolved, onnx);
    }

    #[test]
    fn read_wav_16k_mono_i16_rejects_wrong_format() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("bad.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0_i16).unwrap();
        writer.write_sample(0_i16).unwrap();
        writer.finalize().unwrap();
        let Err(err) = read_wav_16k_mono_i16(&path) else {
            panic!("stereo @ 44.1k should fail");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("must be 16 kHz mono"), "got: {msg}");
    }

    fn summary(reason: TerminationReason, written: u64, trimmed: u64) -> CaptureSummary {
        CaptureSummary {
            output: std::path::PathBuf::from("/tmp/out.wav"),
            samples_written: written,
            trimmed_samples: trimmed,
            terminated_by: reason,
        }
    }

    #[test]
    fn format_capture_summary_idle_termination_mentions_silence() {
        let s = format_capture_summary(&summary(TerminationReason::Idle, 16_000, 3_200));
        assert!(s.contains("silence threshold reached"));
        assert!(
            s.contains("1.00s"),
            "16000 samples @ 16kHz = 1.00s; got: {s}"
        );
        assert!(s.contains("16000 samples"));
        assert!(s.contains("3200 trimmed"));
    }

    #[test]
    fn format_capture_summary_signal_termination_mentions_ctrl_c_or_deadline() {
        let s = format_capture_summary(&summary(TerminationReason::Signal, 48_000, 0));
        assert!(s.contains("Ctrl-C or max-secs deadline"));
        assert!(
            s.contains("3.00s"),
            "48000 samples @ 16kHz = 3.00s; got: {s}"
        );
    }

    #[test]
    fn format_capture_summary_source_exhausted_mentions_source() {
        let s = format_capture_summary(&summary(TerminationReason::SourceExhausted, 8_000, 0));
        assert!(s.contains("audio source ended"));
        assert!(s.contains("0.50s"));
    }

    #[test]
    fn samples_to_seconds_round_trips_at_16k() {
        assert!((samples_to_seconds(0) - 0.0).abs() < f64::EPSILON);
        assert!((samples_to_seconds(16_000) - 1.0).abs() < f64::EPSILON);
        assert!((samples_to_seconds(8_000) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn embed_dim_hint_is_256() {
        assert_eq!(embed_dim_hint(), 256);
    }

    #[test]
    fn read_wav_16k_mono_i16_decodes_ok_when_format_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("ok.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for s in [100_i16, 200, 300, 400] {
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
        let samples = read_wav_16k_mono_i16(&path).unwrap();
        assert_eq!(samples, vec![100, 200, 300, 400]);
    }
}
