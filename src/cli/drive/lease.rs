//! CLI commands for `omni-dev drive lease` — the Drive write lease
//! ([ADR-0080](../../../docs/adrs/adr-0080.md)).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::lease::acquire::{self, AcquireOptions, AcquireResult};
use crate::drive::lease::authenticate::{self, AuthPolicy};
use crate::drive::lease::ledger;

/// Default lease expiry when `--expiry-minutes` is not given (ADR-0080 §5).
const DEFAULT_EXPIRY_MINUTES: i64 = 30;

/// The accepted `--expiry-minutes` range: at least a minute (0 or negative
/// would mint a lease that is already expired the instant Touch ID
/// succeeds, spending a real prompt on a token nothing could ever use), and
/// capped at 24 hours (ADR-0080 §5 frames expiry as bounding "the window in
/// which the agent may write without a fresh Touch ID prompt" — a value far
/// past that intent is far more likely a typo than a deliberate choice, and
/// an unbounded `i64` risks overflowing `chrono::Duration::minutes`).
const MAX_EXPIRY_MINUTES: i64 = 24 * 60;

/// Validates `--expiry-minutes` before anything else runs — no network
/// call, no Touch ID prompt — so a bad value is a clean parse error instead
/// of either an instantly-dead lease (a non-positive value) or a panic
/// (`chrono::Duration::minutes` overflowing on an extreme one).
fn parse_expiry_minutes(s: &str) -> Result<i64, String> {
    let value: i64 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a valid integer"))?;
    if !(1..=MAX_EXPIRY_MINUTES).contains(&value) {
        return Err(format!(
            "must be between 1 and {MAX_EXPIRY_MINUTES} (24 hours), got {value}"
        ));
    }
    Ok(value)
}

/// The Drive write lease: acquire a Touch ID-authorised backup token before
/// a content-mutating write.
#[derive(Parser)]
pub struct LeaseCommand {
    #[command(subcommand)]
    action: LeaseAction,
}

#[derive(Subcommand)]
enum LeaseAction {
    /// Backs up a file and mints a lease token, prompting for device-owner
    /// authentication (Touch ID or the account password).
    Acquire(AcquireCommand),
}

impl LeaseCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.action {
            LeaseAction::Acquire(cmd) => cmd.execute(client).await,
        }
    }
}

/// Backs up `FILE_ID`'s current content and mints a lease token
/// (ADR-0080 §2). Refuses a Google-native document (Docs/Sheets/Slides) —
/// native-file leases land in a later phase.
#[derive(Parser)]
pub struct AcquireCommand {
    /// Drive file id to lease (from `drive search`, or the `id` segment of
    /// a Drive URL).
    pub file_id: String,

    /// Local directory byte backups are written under. Defaults to
    /// `<state dir>/omni-dev/drive-backups`.
    #[arg(long, value_name = "PATH")]
    pub backup_dir: Option<std::path::PathBuf>,

    /// Minutes the lease stays live once authorised. A write never
    /// extends this — a fresh window means a fresh `drive lease acquire`
    /// (ADR-0080 §5).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_EXPIRY_MINUTES, value_parser = parse_expiry_minutes)]
    pub expiry_minutes: i64,

    /// Require Touch ID specifically, failing outright rather than
    /// falling back to the account password (ADR-0080 §7). Needs Touch ID
    /// hardware; the default policy works on any Mac.
    #[arg(long)]
    pub biometrics_only: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AcquireCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let backup_dir = match self.backup_dir {
            Some(dir) => dir,
            None => default_backup_dir()?,
        };
        let ledger_path = ledger::ledger_path()?;
        let opts = AcquireOptions {
            file_id: self.file_id,
            backup_dir,
            expiry: chrono::Duration::minutes(self.expiry_minutes),
            auth_policy: if self.biometrics_only {
                AuthPolicy::BiometricsOnly
            } else {
                AuthPolicy::DeviceOwner
            },
            ledger_path,
        };
        let authenticator = authenticate::platform_authenticator();
        let result = acquire::acquire(client, &opts, authenticator.as_ref()).await;
        if output_as(&result, &self.output)? {
            return Ok(());
        }
        print_result(&result);
        Ok(())
    }
}

/// `<state dir>/omni-dev/drive-backups` — a sibling of the request log and
/// lease ledger, same posture.
fn default_backup_dir() -> Result<std::path::PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("could not resolve the state/data directory for the default backup directory")?;
    Ok(base.join("omni-dev").join("drive-backups"))
}

fn print_result(result: &AcquireResult) {
    match result {
        AcquireResult::Acquired {
            token,
            expires_at,
            backup_path,
        } => {
            println!("{token}");
            eprintln!(
                "Backed up to {} (expires {expires_at})",
                backup_path.display()
            );
        }
        AcquireResult::RefusedNativeDocument => {
            eprintln!(
                "Refused: this is a Google-native document (Doc/Sheet/Slide) — native-file \
                 leases are not yet supported"
            );
        }
        AcquireResult::Denied { detail } => eprintln!("Denied: {detail}"),
        AcquireResult::Unavailable { detail } => eprintln!("Unavailable: {detail}"),
        AcquireResult::Failed { detail } => eprintln!("Failed: {detail}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: Wrapped,
    }

    #[derive(Subcommand)]
    enum Wrapped {
        Lease(LeaseCommand),
    }

    fn parse(args: &[&str]) -> AcquireCommand {
        let mut full = vec!["omni-dev", "lease"];
        full.extend_from_slice(args);
        match Wrapper::try_parse_from(full).unwrap().cmd {
            Wrapped::Lease(cmd) => match cmd.action {
                LeaseAction::Acquire(acquire) => acquire,
            },
        }
    }

    /// Like [`parse`], but for a value expected to fail `clap` validation.
    fn parse_err(args: &[&str]) -> String {
        let mut full = vec!["omni-dev", "lease"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full)
            .err()
            .expect("expected a parse error")
            .to_string()
    }

    #[test]
    fn defaults_are_sane() {
        let cmd = parse(&["acquire", "file1"]);
        assert_eq!(cmd.file_id, "file1");
        assert!(cmd.backup_dir.is_none());
        assert_eq!(cmd.expiry_minutes, DEFAULT_EXPIRY_MINUTES);
        assert!(!cmd.biometrics_only);
    }

    #[test]
    fn expiry_minutes_boundary_values_are_accepted() {
        assert_eq!(
            parse(&["acquire", "file1", "--expiry-minutes", "1"]).expiry_minutes,
            1
        );
        assert_eq!(
            parse(&["acquire", "file1", "--expiry-minutes", "1440"]).expiry_minutes,
            1440
        );
    }

    #[test]
    fn expiry_minutes_zero_is_rejected() {
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "0"]);
        assert!(err.contains("must be between 1 and 1440"), "{err}");
    }

    #[test]
    fn expiry_minutes_negative_is_rejected() {
        // clap treats a leading `-` as looking like a flag before our own
        // `value_parser` ever runs (`-- -5` is the escape hatch) — still a
        // clean rejection, just with clap's own message rather than ours.
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "-5"]);
        assert!(err.contains("unexpected argument"), "{err}");
    }

    #[test]
    fn expiry_minutes_past_the_cap_is_rejected() {
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "1441"]);
        assert!(err.contains("must be between 1 and 1440"), "{err}");
    }

    #[test]
    fn expiry_minutes_over_the_cap_but_still_a_valid_i64_is_rejected_cleanly() {
        // Large enough to be a plausible typo, small enough to still parse
        // as `i64` — exercises the range check specifically, as opposed to
        // the integer-parse failure below.
        let err = parse_err(&["acquire", "file1", "--expiry-minutes", "999999999999999999"]);
        assert!(err.contains("must be between 1 and 1440"), "{err}");
    }

    #[test]
    fn expiry_minutes_past_i64_range_is_a_clean_parse_error_not_a_panic() {
        // Would overflow `chrono::Duration::minutes` if it ever reached
        // that call — must be rejected as a plain integer-parse failure
        // instead, well before the range check.
        let err = parse_err(&[
            "acquire",
            "file1",
            "--expiry-minutes",
            "99999999999999999999",
        ]);
        assert!(err.contains("not a valid integer"), "{err}");
    }

    #[test]
    fn flags_parse() {
        let cmd = parse(&[
            "acquire",
            "file1",
            "--backup-dir",
            "/tmp/backups",
            "--expiry-minutes",
            "10",
            "--biometrics-only",
        ]);
        assert_eq!(
            cmd.backup_dir,
            Some(std::path::PathBuf::from("/tmp/backups"))
        );
        assert_eq!(cmd.expiry_minutes, 10);
        assert!(cmd.biometrics_only);
    }
}
