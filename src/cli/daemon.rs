//! `omni-dev daemon` — supervise the long-lived daemon and its services.

pub(crate) mod control;
pub(crate) mod restart;
pub(crate) mod run;
pub(crate) mod start;
pub(crate) mod status;
pub(crate) mod stop;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Daemon: host long-lived services (e.g. the browser bridge) under one
/// supervised, menu-bar-controllable process.
#[derive(Parser)]
pub struct DaemonCommand {
    /// The daemon subcommand to execute.
    #[command(subcommand)]
    pub command: DaemonSubcommands,
}

/// Daemon subcommands.
#[derive(Subcommand)]
pub enum DaemonSubcommands {
    /// Runs the daemon in the foreground (the process launchd execs).
    Run(run::RunCommand),
    /// Starts the daemon in the background.
    Start(start::StartCommand),
    /// Stops the running daemon.
    Stop(stop::StopCommand),
    /// Restarts the daemon.
    Restart(restart::RestartCommand),
    /// Reports daemon and per-service status.
    Status(status::StatusCommand),
}

impl DaemonCommand {
    /// Executes the daemon command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            DaemonSubcommands::Run(cmd) => cmd.execute().await,
            DaemonSubcommands::Start(cmd) => cmd.execute().await,
            DaemonSubcommands::Stop(cmd) => cmd.execute().await,
            DaemonSubcommands::Restart(cmd) => cmd.execute().await,
            DaemonSubcommands::Status(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Mirrors the `omni-dev daemon` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: DaemonSubcommands,
    }

    fn parse(args: &[&str]) -> DaemonSubcommands {
        let mut full = vec!["omni-dev"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full).unwrap().cmd
    }

    #[test]
    fn parses_all_subcommands() {
        assert!(matches!(parse(&["run"]), DaemonSubcommands::Run(_)));
        assert!(matches!(parse(&["start"]), DaemonSubcommands::Start(_)));
        assert!(matches!(parse(&["stop"]), DaemonSubcommands::Stop(_)));
        assert!(matches!(parse(&["restart"]), DaemonSubcommands::Restart(_)));
        assert!(matches!(parse(&["status"]), DaemonSubcommands::Status(_)));
    }

    #[test]
    fn socket_override_parses() {
        let DaemonSubcommands::Run(cmd) = parse(&["run", "--socket", "/tmp/x.sock"]) else {
            panic!("expected run");
        };
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/x.sock")));
    }

    #[test]
    fn status_json_flag_parses() {
        let DaemonSubcommands::Status(cmd) = parse(&["status", "--json"]) else {
            panic!("expected status");
        };
        assert!(cmd.json);
    }

    #[test]
    fn socket_defaults_to_none() {
        let DaemonSubcommands::Status(cmd) = parse(&["status"]) else {
            panic!("expected status");
        };
        assert!(cmd.socket.is_none());
        assert!(!cmd.json);
    }
}
