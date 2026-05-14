//! Voice-related CLI commands.
//!
//! Provider-namespaced (`capture`, `transcribe` today; later: `listen`,
//! `review`). Per-subcommand argument structs live in submodules to keep
//! help text and parse logic local to each command.

pub mod capture;
pub mod install_model;
pub mod reflect;
pub mod review;
pub mod transcribe;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Voice capture and processing operations.
#[derive(Parser)]
pub struct VoiceCommand {
    /// The voice subcommand to execute.
    #[command(subcommand)]
    pub command: VoiceSubcommands,
}

/// Voice subcommands.
#[derive(Subcommand)]
pub enum VoiceSubcommands {
    /// Captures audio from a microphone to a 16 kHz mono WAV file.
    Capture(capture::CaptureCommand),
    /// Transcribes a 16 kHz mono WAV file to JSONL or markdown.
    Transcribe(transcribe::TranscribeCommand),
    /// Reflects on a transcript and emits reflection events.
    Reflect(reflect::ReflectCommand),
    /// Reconciles a session's events.jsonl into materialized markdown.
    Review(review::ReviewCommand),
    /// Downloads the Whisper tiny.en model files for the `whisper-candle`
    /// backend into `~/.omni-dev/voice/models/whisper-tiny.en/`.
    InstallModel(install_model::InstallModelCommand),
}

impl VoiceCommand {
    /// Dispatches to the selected voice subcommand.
    ///
    /// Async because `reflect` invokes Claude via an async
    /// [`crate::claude::ai::AiClient`]. Sync arms just `.await` an
    /// immediately-ready value via `async {…}`.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            VoiceSubcommands::Capture(cmd) => cmd.execute(),
            VoiceSubcommands::Transcribe(cmd) => cmd.execute(),
            VoiceSubcommands::Reflect(cmd) => cmd.execute().await,
            VoiceSubcommands::Review(cmd) => cmd.execute(),
            VoiceSubcommands::InstallModel(cmd) => cmd.execute(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    #[test]
    fn voice_subcommands_capture_variant() {
        let cmd = VoiceCommand {
            command: VoiceSubcommands::Capture(capture::CaptureCommand {
                idle_after: 5,
                output: None,
                device: None,
            }),
        };
        assert!(matches!(cmd.command, VoiceSubcommands::Capture(_)));
    }

    #[test]
    fn voice_subcommands_transcribe_variant() {
        let cmd = VoiceCommand {
            command: VoiceSubcommands::Transcribe(transcribe::TranscribeCommand {
                wav: PathBuf::from("/tmp/x.wav"),
                backend: None,
                model: None,
                format: None,
            }),
        };
        assert!(matches!(cmd.command, VoiceSubcommands::Transcribe(_)));
    }

    #[test]
    fn voice_subcommands_reflect_variant() {
        let cmd = VoiceCommand {
            command: VoiceSubcommands::Reflect(reflect::ReflectCommand {
                transcript: Some(PathBuf::from("/tmp/t.jsonl")),
                session: None,
            }),
        };
        assert!(matches!(cmd.command, VoiceSubcommands::Reflect(_)));
    }

    #[test]
    fn voice_subcommands_install_model_variant() {
        let cmd = VoiceCommand {
            command: VoiceSubcommands::InstallModel(install_model::InstallModelCommand {
                dest: None,
                force: false,
            }),
        };
        assert!(matches!(cmd.command, VoiceSubcommands::InstallModel(_)));
    }

    #[tokio::test]
    async fn voice_command_dispatches_install_model_via_execute() {
        // Drives VoiceCommand::execute through the InstallModel arm
        // end-to-end: covers the async dispatch in cli/voice.rs and the
        // stderr-locking wrapper in install_model::execute. Uses a
        // pre-staged tempdir so the idempotent early-return path keeps
        // the test off the network.
        use crate::voice::models::REQUIRED_FILES;

        let tmp = tempfile::TempDir::new().unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(tmp.path().join(f), b"placeholder").unwrap();
        }

        let cmd = VoiceCommand {
            command: VoiceSubcommands::InstallModel(install_model::InstallModelCommand {
                dest: Some(tmp.path().to_path_buf()),
                force: false,
            }),
        };
        cmd.execute()
            .await
            .expect("install-model dispatch should succeed on pre-staged dir");
    }
}
