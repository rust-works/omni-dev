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
    #[arg(long, value_name = "N", default_value_t = DEFAULT_EXPIRY_MINUTES)]
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
#[allow(clippy::unwrap_used)]
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

    #[test]
    fn defaults_are_sane() {
        let cmd = parse(&["acquire", "file1"]);
        assert_eq!(cmd.file_id, "file1");
        assert!(cmd.backup_dir.is_none());
        assert_eq!(cmd.expiry_minutes, DEFAULT_EXPIRY_MINUTES);
        assert!(!cmd.biometrics_only);
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
