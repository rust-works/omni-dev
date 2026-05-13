//! Voice-related CLI commands.
//!
//! Provider-namespaced (`capture`, `transcribe` today; later: `listen`,
//! `review`). Per-subcommand argument structs live in submodules to keep
//! help text and parse logic local to each command.

pub mod capture;
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
}

impl VoiceCommand {
    /// Dispatches to the selected voice subcommand.
    pub fn execute(self) -> Result<()> {
        match self.command {
            VoiceSubcommands::Capture(cmd) => cmd.execute(),
            VoiceSubcommands::Transcribe(cmd) => cmd.execute(),
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
                format: None,
            }),
        };
        assert!(matches!(cmd.command, VoiceSubcommands::Transcribe(_)));
    }
}
