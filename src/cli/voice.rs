//! Voice-related CLI commands.
//!
//! Provider-namespaced (only `capture` today; later: `listen`, `transcribe`).
//! Per-subcommand argument structs live in submodules to keep help text
//! and parse logic local to each command.

pub mod capture;

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
}

impl VoiceCommand {
    /// Dispatches to the selected voice subcommand.
    pub fn execute(self) -> Result<()> {
        match self.command {
            VoiceSubcommands::Capture(cmd) => cmd.execute(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
}
