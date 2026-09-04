//! `omni-dev worktrees ui` — a full-screen terminal UI for the worktrees tree
//! (issue #1585).
//!
//! Phase 1: a read-only, live-updating view of every repo/worktree the
//! daemon knows about, superseding `worktrees tree` for interactive use.
//! Actions, embedded terminals, tabs/splits and the full VS Code-parity
//! surface land in later phases — see the issue and
//! `/Users/jky/.claude/plans/unified-snacking-dragonfly.md` for the full plan.

mod ahead_behind;
mod client;
mod hub;
mod local_state;
mod render;
mod row_colors;
mod supervisor;
mod view_model;
mod wire;

use std::io::Stdout;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use crate::daemon::server;
use hub::ViewModelHandle;

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
        let handle = hub::spawn(socket, cancel.clone());

        let mut guard = TerminalGuard::enter()?;
        let result = run(&mut guard.terminal, handle).await;
        drop(guard); // always restores the terminal, even on an error path
        cancel.cancel();
        result
    }
}

/// RAII guard: enters raw mode, the alternate screen, and mouse capture on
/// construction, and unconditionally restores the terminal on drop — so a
/// panic or an early `?` return never leaves the user's shell in raw mode.
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
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(e) => {
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
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
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

/// Phase 1's event loop: redraws on every hub update, quits on `q`/`Esc`/
/// Ctrl-C. Row navigation, mouse handling, actions and embedded terminals are
/// later phases (issue #1585 §3-§5); this loop's only job today is to prove
/// the data layer end to end with a real, live, quittable terminal app.
async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut handle: ViewModelHandle,
) -> Result<()> {
    let mut events = EventStream::new();
    // The hub bumps `generation` on every rebuild; skipping a redraw when it
    // hasn't moved since the last frame is the cheap check the view model's
    // doc comment promises, rather than a deep diff of the tree every tick.
    let mut last_drawn_generation: Option<u64> = None;
    loop {
        let view = handle.view.borrow_and_update().clone();
        if last_drawn_generation != Some(view.generation) {
            terminal.draw(|frame| render::draw(frame, &view))?;
            last_drawn_generation = Some(view.generation);
        }

        tokio::select! {
            biased;
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            changed = handle.view.changed() => {
                if changed.is_err() {
                    break; // the hub actor is gone
                }
            }
            // A fallback tick so a "reconnecting"/"polling" status bar redraws
            // even between snapshots and keypresses.
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
    Ok(())
}
