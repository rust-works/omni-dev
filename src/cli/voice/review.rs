//! `omni-dev voice review` — reconcile a session's `events.jsonl`
//! into materialised markdown.
//!
//! Writes `todos.md` / `decisions.md` and (optionally) renders the
//! transcript. See [`crate::voice::review`] for the driver and
//! [`crate::voice::reconcile`] for the pure logic.

use std::io::Write;

use anyhow::Result;
use clap::Parser;

use crate::voice::clock::SystemClock;
use crate::voice::det::SystemUlidRng;
use crate::voice::review::{run_review, ReviewOptions, What};

/// Reconciles a session's reflection log into materialised markdown.
///
/// Reads `~/.omni-dev/voice/<session-id>/events.jsonl`, computes
/// projections per #799's reconciliation invariants, applies TTL
/// expiry against the session's class-default TTLs, and writes
/// `todos.md` / `decisions.md` under the session directory. Any
/// synthesised `item.expire { reason: ttl }` events are appended back
/// to `events.jsonl`.
#[derive(Parser)]
pub struct ReviewCommand {
    /// Session id under `~/.omni-dev/voice/<id>/`.
    #[arg(value_name = "SESSION_ID")]
    pub session_id: String,

    /// Which artefact to materialise. `all` (default) writes both
    /// markdown files and applies the TTL pass. `transcript` renders
    /// `transcript.jsonl` to stdout instead.
    #[arg(long, value_enum, default_value_t = What::All)]
    pub what: What,
}

impl ReviewCommand {
    /// Executes the review command. Sync because reconciliation does
    /// no AI calls — the `voice` dispatch wraps this in an immediately
    /// ready future.
    pub fn execute(self) -> Result<()> {
        let opts = ReviewOptions {
            session_id: self.session_id,
            what: self.what,
            ulid_rng: Box::new(SystemUlidRng),
            clock: Box::new(SystemClock),
            session_root_override: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        run_review(opts, &mut buf)?;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&buf)?;
        stdout.flush()?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        review: ReviewCommand,
    }

    #[test]
    fn parses_session_id_and_defaults_to_all() {
        let cli = TestCli::try_parse_from(["test", "demo"]).unwrap();
        assert_eq!(cli.review.session_id, "demo");
        assert_eq!(cli.review.what, What::All);
    }

    #[test]
    fn parses_what_flag() {
        let cli = TestCli::try_parse_from(["test", "demo", "--what", "todos"]).unwrap();
        assert_eq!(cli.review.what, What::Todos);
    }

    #[test]
    fn rejects_unknown_what_value() {
        let result = TestCli::try_parse_from(["test", "demo", "--what", "garbage"]);
        let Err(err) = result else {
            panic!("expected parse failure for unknown --what value");
        };
        assert!(err.to_string().contains("invalid value"));
    }

    #[test]
    fn rejects_missing_session_id() {
        let result = TestCli::try_parse_from(["test"]);
        let Err(err) = result else {
            panic!("expected parse failure when SESSION_ID is missing");
        };
        assert!(err.to_string().to_lowercase().contains("session"));
    }
}
