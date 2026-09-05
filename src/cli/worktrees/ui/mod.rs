//! `omni-dev worktrees ui` — a full-screen terminal UI for the worktrees tree
//! (issue #1585).
//!
//! Phase 1 was a read-only, live-updating view of every repo/worktree the
//! daemon knows about, superseding `worktrees tree` for interactive use.
//! Phase 2 added row navigation/marking, an action menu, and the daemon-free
//! parity commands plus the two-phase close flow. Phase 3 (this code) hosts
//! one embedded terminal tab on the right — a real PTY running the user's
//! shell or `claude` (through `omni-dev claude-wrap`, so the session reports
//! authoritative state) — driven by `alacritty_terminal`. Tabs/splits, the
//! mouse/selection contract and the rest of the VS Code-parity surface stay
//! in later phases — see the issue and
//! `/Users/jky/.claude/plans/unified-snacking-dragonfly.md` for the full plan.

mod actions;
mod ahead_behind;
mod app;
mod client;
mod clipboard;
mod hub;
mod keys;
mod local_state;
mod popup;
mod render;
mod row_colors;
mod supervisor;
mod terminal;
mod tree;
mod view_model;
mod wire;

use std::io::Stdout;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use crate::daemon::server;
use actions::Dispatcher;
use client::WorktreesClient;

/// Launches the full-screen terminal UI for the worktrees tree.
#[derive(Parser)]
pub struct UiCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl UiCommand {
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let cancel = CancellationToken::new();
        let handle = hub::spawn(socket.clone(), cancel.clone());
        let commands = handle.commands.clone();
        let dispatcher = Dispatcher::new(WorktreesClient::new(socket), commands.clone());

        let mut guard = TerminalGuard::enter()?;
        let result = app::run(&mut guard.terminal, handle, dispatcher, commands).await;
        drop(guard); // always restores the terminal, even on an error path
        cancel.cancel();
        result
    }
}

/// RAII guard: enters raw mode, the alternate screen, mouse capture, and
/// bracketed paste on construction, and unconditionally restores the terminal
/// on drop — so a panic or an early `?` return never leaves the user's shell
/// in raw mode.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Undoes every step of `enter()` that already succeeded before a later
    /// step fails — `Drop` only runs for a fully-constructed `Self`, so a
    /// partial failure here (e.g. `execute!` after raw mode is already on)
    /// would otherwise leave the invoking shell in raw mode / the alternate
    /// screen with no guard left to clean it up.
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        if let Err(e) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            let _ = execute!(
                std::io::stdout(),
                DisableBracketedPaste,
                LeaveAlternateScreen,
                DisableMouseCapture
            );
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(e) => {
                let _ = execute!(
                    std::io::stdout(),
                    DisableBracketedPaste,
                    LeaveAlternateScreen,
                    DisableMouseCapture
                );
                let _ = disable_raw_mode();
                return Err(e.into());
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}
