//! Shared confirmation and dry-run helper for destructive Atlassian CLI commands.

use std::io::{self, BufRead, Write};

use anyhow::Result;

/// Outcome of a destructive-action guard.
#[derive(Debug, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Caller should proceed with the destructive operation.
    Proceed,
    /// User declined the prompt; the guard already printed a cancellation
    /// notice. Caller should return `Ok(())` without performing the action.
    Cancelled,
    /// `--dry-run` was set; the guard already printed the preview. Caller
    /// should return `Ok(())` without invoking any API.
    DryRun,
}

/// Configuration for a single destructive-action guard invocation.
pub struct GuardOptions<'a> {
    /// The yes/no prompt shown to the user (e.g. "Delete PROJ-123 (Fix login)? [y/N] ").
    pub prompt: &'a str,
    /// The preview message printed when `--dry-run` is set
    /// (e.g. "Would delete PROJ-123 (Fix login).").
    pub dry_run_message: &'a str,
    /// Skip the interactive prompt.
    pub force: bool,
    /// Print `dry_run_message` and return without prompting or calling the API.
    pub dry_run: bool,
}

/// Guards a destructive operation, prompting on stdin/stdout as needed.
///
/// `--dry-run` takes precedence over `--force`: a dry-run never invokes the API,
/// even when `--force` is also set. This lets users sanity-check a scripted
/// `--force` invocation by adding `--dry-run` without removing the force flag.
pub fn guard_destructive(opts: &GuardOptions<'_>) -> Result<GuardOutcome> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    guard_destructive_with_io(opts, &mut reader, &mut writer)
}

/// Inner form taking explicit reader/writer for unit tests.
pub fn guard_destructive_with_io(
    opts: &GuardOptions<'_>,
    reader: &mut dyn BufRead,
    writer: &mut dyn Write,
) -> Result<GuardOutcome> {
    if opts.dry_run {
        writeln!(writer, "{}", opts.dry_run_message)?;
        return Ok(GuardOutcome::DryRun);
    }

    if opts.force {
        return Ok(GuardOutcome::Proceed);
    }

    write!(writer, "{}", opts.prompt)?;
    writer.flush()?;

    let mut answer = String::new();
    reader.read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        Ok(GuardOutcome::Proceed)
    } else {
        writeln!(writer, "Cancelled.")?;
        Ok(GuardOutcome::Cancelled)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn opts<'a>(prompt: &'a str, dry_run_message: &'a str) -> GuardOptions<'a> {
        GuardOptions {
            prompt,
            dry_run_message,
            force: false,
            dry_run: false,
        }
    }

    // ── dry-run precedence ─────────────────────────────────────────

    #[test]
    fn dry_run_returns_dry_run_and_prints_preview() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let mut o = opts("Delete? ", "Would delete X.");
        o.dry_run = true;

        let outcome = guard_destructive_with_io(&o, &mut input, &mut output).unwrap();
        assert_eq!(outcome, GuardOutcome::DryRun);
        assert_eq!(String::from_utf8(output).unwrap(), "Would delete X.\n");
    }

    #[test]
    fn dry_run_wins_over_force() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let mut o = opts("Delete? ", "Would delete X.");
        o.force = true;
        o.dry_run = true;

        let outcome = guard_destructive_with_io(&o, &mut input, &mut output).unwrap();
        assert_eq!(outcome, GuardOutcome::DryRun);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Would delete X."));
    }

    // ── force ──────────────────────────────────────────────────────

    #[test]
    fn force_returns_proceed_without_io() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let mut o = opts("Delete? ", "Would delete X.");
        o.force = true;

        let outcome = guard_destructive_with_io(&o, &mut input, &mut output).unwrap();
        assert_eq!(outcome, GuardOutcome::Proceed);
        assert!(output.is_empty());
    }

    // ── prompt answers ─────────────────────────────────────────────

    #[test]
    fn yes_lowercase_proceeds() {
        let mut input = Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        let outcome =
            guard_destructive_with_io(&opts("Delete? ", "Would delete."), &mut input, &mut output)
                .unwrap();
        assert_eq!(outcome, GuardOutcome::Proceed);
        assert_eq!(String::from_utf8(output).unwrap(), "Delete? ");
    }

    #[test]
    fn yes_uppercase_proceeds() {
        let mut input = Cursor::new(b"Y\n".to_vec());
        let mut output = Vec::<u8>::new();
        let outcome =
            guard_destructive_with_io(&opts("Delete? ", "Would delete."), &mut input, &mut output)
                .unwrap();
        assert_eq!(outcome, GuardOutcome::Proceed);
    }

    #[test]
    fn yes_with_whitespace_proceeds() {
        let mut input = Cursor::new(b"  y  \n".to_vec());
        let mut output = Vec::<u8>::new();
        let outcome =
            guard_destructive_with_io(&opts("Delete? ", "Would delete."), &mut input, &mut output)
                .unwrap();
        assert_eq!(outcome, GuardOutcome::Proceed);
    }

    #[test]
    fn no_cancels_and_prints_notice() {
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        let outcome =
            guard_destructive_with_io(&opts("Delete? ", "Would delete."), &mut input, &mut output)
                .unwrap();
        assert_eq!(outcome, GuardOutcome::Cancelled);
        assert!(String::from_utf8(output).unwrap().contains("Cancelled."));
    }

    #[test]
    fn empty_answer_cancels() {
        let mut input = Cursor::new(b"\n".to_vec());
        let mut output = Vec::<u8>::new();
        let outcome =
            guard_destructive_with_io(&opts("Delete? ", "Would delete."), &mut input, &mut output)
                .unwrap();
        assert_eq!(outcome, GuardOutcome::Cancelled);
    }

    #[test]
    fn random_text_cancels() {
        let mut input = Cursor::new(b"maybe\n".to_vec());
        let mut output = Vec::<u8>::new();
        let outcome =
            guard_destructive_with_io(&opts("Delete? ", "Would delete."), &mut input, &mut output)
                .unwrap();
        assert_eq!(outcome, GuardOutcome::Cancelled);
    }
}
