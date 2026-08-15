//! Small terminal helpers shared by the raw-mode secret prompts in
//! `cli::ai::chat`, `cli::gmail::auth`, and `cli::drive::auth`.

use crossterm::terminal::disable_raw_mode;

/// Guard that disables raw mode on drop.
pub struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
