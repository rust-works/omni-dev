//! Drive CLI commands.

pub(crate) mod account;
pub(crate) mod auth;
pub(crate) mod dedupe;
pub(crate) mod format;
pub(crate) mod helpers;
pub(crate) mod read;
pub(crate) mod search;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::drive::account::DRIVE_ACCOUNT_ENV;
use crate::drive::client::DriveClient;

/// Drive: search and read Google Drive files via OAuth2 (read-only).
#[derive(Parser)]
pub struct DriveCommand {
    /// Selects a named Drive account configured in
    /// `~/.omni-dev/settings.json` (AWS-CLI style, mirrors the top-level
    /// `--profile`) for this invocation.
    ///
    /// Orthogonal to `--profile`: switching the Drive account never changes
    /// which profile is active, and vice versa (see
    /// [ADR-0066](../../../docs/adrs/adr-0066.md),
    /// [ADR-0069](../../../docs/adrs/adr-0069.md)). Overrides
    /// `OMNI_DEV_DRIVE_ACCOUNT`. Scoped to the `drive` subtree — not usable
    /// before the `drive` subcommand name, only after it, so it can't
    /// collide with an unrelated subcommand's own `--account` flag.
    #[arg(long, global = true, value_name = "NAME")]
    pub account: Option<String>,
    /// The Drive subcommand to execute.
    #[command(subcommand)]
    pub command: DriveSubcommands,
}

/// Drive subcommands.
#[derive(Subcommand)]
pub enum DriveSubcommands {
    /// Manages Drive OAuth2 credentials.
    Auth(auth::AuthCommand),
    /// Manages named Drive accounts.
    Account(account::AccountCommand),
    /// Searches Drive files.
    Search(search::SearchCommand),
    /// Reads a single Drive file's metadata or content.
    Read(read::ReadCommand),
    /// Finds Drive files sharing the same content hash.
    Dedupe(dedupe::DedupeCommand),
}

impl DriveCommand {
    /// Executes the Drive command. `auth`/`account` must run without a
    /// resolved client (they manage credentials/account selection); every
    /// other subcommand resolves one shared client **once** here and
    /// threads it down via [`DriveSubcommands::dispatch`].
    pub async fn execute(self) -> Result<()> {
        // Propagates --account to DRIVE_ACCOUNT_ENV for the duration of this
        // call only (crate::drive::account::resolve_account reads it),
        // mirroring GmailCommand::execute. Set *before* matching Auth/
        // Account: those subcommands need the resolved account too (e.g.
        // `drive auth login --account work`). The guard restores the
        // ambient value (or removes the var) on drop at the end of this
        // function, so execute() is safe to call more than once per process
        // (#1538).
        let _account_guard = self
            .account
            .as_ref()
            .map(|account| crate::utils::env::ScopedEnvVar::set(DRIVE_ACCOUNT_ENV, account));

        match self.command {
            DriveSubcommands::Auth(cmd) => cmd.execute().await,
            DriveSubcommands::Account(cmd) => cmd.execute(),
            command => {
                let client = helpers::create_client()?;
                command.dispatch(&client).await
            }
        }
    }
}

impl DriveSubcommands {
    /// Routes a non-`Auth`/`Account` subcommand against the shared client.
    /// The `Auth`/`Account` arms are unreachable: both are handled before
    /// client resolution in [`DriveCommand::execute`].
    async fn dispatch(self, client: &DriveClient) -> Result<()> {
        match self {
            Self::Auth(_) => unreachable!("Auth is dispatched before client resolution"),
            Self::Account(_) => unreachable!("Account is dispatched before client resolution"),
            Self::Search(cmd) => cmd.execute(client).await,
            Self::Read(cmd) => cmd.execute(client).await,
            Self::Dedupe(cmd) => cmd.execute(client).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::drive::format::OutputFormat;
    use crate::drive::auth::{DriveCredentials, DriveScope};
    use crate::utils::secret::Secret;

    fn dead_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: DriveScope::ReadOnly,
        }
    }

    fn dead_client() -> DriveClient {
        DriveClient::new("http://127.0.0.1:1", &dead_credentials()).unwrap()
    }

    #[tokio::test]
    async fn execute_routes_auth_subcommand_and_surfaces_missing_credentials() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DriveCommand {
            account: None,
            command: DriveSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand { all: false }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn execute_non_auth_subcommand_errors_when_credentials_missing() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DriveCommand {
            account: None,
            command: DriveSubcommands::Search(search::SearchCommand {
                query: "name contains 'report'".to_string(),
                limit: 10,
                output: OutputFormat::Table,
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn execute_restores_account_env_var_after_return() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DriveCommand {
            account: Some("work".to_string()),
            command: DriveSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        cmd.execute().await.unwrap();
        assert_eq!(std::env::var(DRIVE_ACCOUNT_ENV).ok(), None);
    }

    #[tokio::test]
    async fn execute_restores_previous_account_env_var_after_return() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        std::env::set_var(DRIVE_ACCOUNT_ENV, "personal");

        let cmd = DriveCommand {
            account: Some("work".to_string()),
            command: DriveSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        cmd.execute().await.unwrap();
        assert_eq!(
            std::env::var(DRIVE_ACCOUNT_ENV).ok().as_deref(),
            Some("personal")
        );
    }

    #[tokio::test]
    async fn execute_does_not_leak_account_across_sequential_calls() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let account_list_cmd = || DriveCommand {
            account: None,
            command: DriveSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };

        let first = DriveCommand {
            account: Some("alpha".to_string()),
            ..account_list_cmd()
        };
        first.execute().await.unwrap();
        assert_eq!(std::env::var(DRIVE_ACCOUNT_ENV).ok(), None);

        // If the first call's value had leaked, this second call — which
        // omits --account entirely — would still see it via the env var.
        account_list_cmd().execute().await.unwrap();
        assert_eq!(std::env::var(DRIVE_ACCOUNT_ENV).ok(), None);
    }

    #[tokio::test]
    async fn execute_absent_account_leaves_ambient_env_var_untouched() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        std::env::set_var(DRIVE_ACCOUNT_ENV, "personal");

        let cmd = DriveCommand {
            account: None,
            command: DriveSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        cmd.execute().await.unwrap();
        assert_eq!(
            std::env::var(DRIVE_ACCOUNT_ENV).ok().as_deref(),
            Some("personal")
        );
    }

    #[tokio::test]
    async fn execute_routes_account_list_without_client_resolution() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DriveCommand {
            account: None,
            command: DriveSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_routes_search() {
        let cmd = DriveSubcommands::Search(search::SearchCommand {
            query: "name contains 'x'".to_string(),
            limit: 10,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_read() {
        let cmd = DriveSubcommands::Read(read::ReadCommand {
            file_id: "f1".to_string(),
            content: false,
            export_mime_type: None,
            out_file: None,
            verify: false,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_dedupe() {
        let cmd = DriveSubcommands::Dedupe(dedupe::DedupeCommand {
            query: "name contains 'x'".to_string(),
            limit: 10,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }
}
