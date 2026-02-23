//! AI commands.

use std::io::{self, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};

/// AI operations.
#[derive(Parser)]
pub struct AiCommand {
    /// The AI subcommand to execute.
    #[command(subcommand)]
    pub command: AiSubcommand,
}

/// AI subcommands.
#[derive(Subcommand)]
pub enum AiSubcommand {
    /// Interactive AI chat session.
    Chat(ChatCommand),
}

impl AiCommand {
    /// Executes the AI command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AiSubcommand::Chat(cmd) => cmd.execute().await,
        }
    }
}

/// Interactive AI chat session.
#[derive(Parser)]
pub struct ChatCommand {
    /// AI model to use (overrides environment configuration).
    #[arg(long)]
    pub model: Option<String>,
}

impl ChatCommand {
    /// Executes the chat command.
    pub async fn execute(self) -> Result<()> {
        let ai_info = crate::utils::preflight::check_ai_credentials(self.model.as_deref())?;
        eprintln!(
            "Connected to {} (model: {})",
            ai_info.provider, ai_info.model
        );
        eprintln!("Enter to send, Shift+Enter for newline, Ctrl+D to exit.\n");

        let client = crate::claude::create_default_claude_client(self.model, None)?;

        chat_loop(&client).await
    }
}

async fn chat_loop(client: &crate::claude::client::ClaudeClient) -> Result<()> {
    let system_prompt = "You are a helpful assistant.";

    loop {
        let input = match read_user_input() {
            Ok(Some(text)) => text,
            Ok(None) => {
                eprintln!("\nGoodbye!");
                break;
            }
            Err(e) => {
                eprintln!("\nInput error: {e}");
                break;
            }
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = client.send_message(system_prompt, trimmed).await?;
        println!("{response}\n");
    }

    Ok(())
}

/// Guard that disables raw mode on drop.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Reads multiline user input with "> " prompt.
///
/// Returns `Ok(Some(text))` on Enter, `Ok(None)` on Ctrl+D/Ctrl+C.
fn read_user_input() -> Result<Option<String>> {
    eprint!("> ");
    io::stderr().flush()?;

    enable_raw_mode()?;
    let _guard = RawModeGuard;

    let mut buffer = String::new();

    loop {
        if let Event::Key(key_event) = event::read()? {
            match key_event.code {
                KeyCode::Enter => {
                    if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                        buffer.push('\n');
                        eprint!("\r\n... ");
                        io::stderr().flush()?;
                    } else {
                        eprint!("\r\n");
                        io::stderr().flush()?;
                        return Ok(Some(buffer));
                    }
                }
                KeyCode::Char('d') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    if buffer.is_empty() {
                        return Ok(None);
                    }
                    eprint!("\r\n");
                    io::stderr().flush()?;
                    return Ok(Some(buffer));
                }
                KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    eprint!("{c}");
                    io::stderr().flush()?;
                }
                KeyCode::Backspace => {
                    if buffer.pop().is_some() {
                        eprint!("\x08 \x08");
                        io::stderr().flush()?;
                    }
                }
                _ => {}
            }
        }
    }
}
