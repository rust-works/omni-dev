//! Gmail CLI commands.

pub(crate) mod account;
pub(crate) mod auth;
pub(crate) mod format;
pub(crate) mod helpers;
pub(crate) mod label;
pub(crate) mod read;
pub(crate) mod search;
pub(crate) mod sync;
pub(crate) mod thread;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::gmail::account::GMAIL_ACCOUNT_ENV;
use crate::gmail::client::GmailClient;

/// Gmail: read Gmail messages, threads, and labels (and, with `gmail.modify`, mutate labels).
#[derive(Parser)]
pub struct GmailCommand {
    /// Selects a named Gmail account configured in
    /// `~/.omni-dev/settings.json` (AWS-CLI style, mirrors the top-level
    /// `--profile`) for this invocation.
    ///
    /// Orthogonal to `--profile`: switching the Gmail account never changes
    /// which profile is active, and vice versa (see
    /// [ADR-0066](../../../docs/adrs/adr-0066.md)). Overrides
    /// `OMNI_DEV_GMAIL_ACCOUNT`. Scoped to the `gmail` subtree — unlike
    /// `--profile`/`--instance` it is not usable before the `gmail`
    /// subcommand name, only after it (`gmail --account NAME <cmd>` or
    /// `gmail <cmd> --account NAME`), so it can't collide with an unrelated
    /// subcommand's own `--account` flag elsewhere in the CLI (e.g.
    /// `snowflake query --account`).
    #[arg(long, global = true, value_name = "NAME")]
    pub account: Option<String>,
    /// The Gmail subcommand to execute.
    #[command(subcommand)]
    pub command: GmailSubcommands,
}

/// Gmail subcommands.
#[derive(Subcommand)]
pub enum GmailSubcommands {
    /// Manages Gmail OAuth2 credentials (mirrors the `gmail_auth_status` MCP tool for `status`).
    Auth(auth::AuthCommand),
    /// Manages named Gmail accounts (mirrors the `gmail_account_list` MCP tool for `list`).
    Account(account::AccountCommand),
    /// Searches Gmail messages (mirrors the `gmail_search` MCP tool).
    Search(search::SearchCommand),
    /// Reads a single Gmail message (mirrors the `gmail_message_read` MCP tool).
    Read(read::ReadCommand),
    /// Reads a Gmail thread (mirrors the `gmail_thread_read` MCP tool).
    Thread(thread::ThreadCommand),
    /// Manages Gmail labels (mirrors the `gmail_label_list` MCP tool; `add`/`remove` are CLI-only in Phase 1).
    Label(label::LabelCommand),
    /// Maintains a durable local archive of a mailbox (CLI-only; no MCP equivalent).
    Sync(sync::SyncCommand),
}

impl GmailCommand {
    /// Executes the Gmail command.
    ///
    /// `auth` manages credentials and must run without them; `account`
    /// manages which named account is selected and must equally run
    /// without a resolved client (`import-legacy`'s whole point is working
    /// in a pre-migration state). Every other subcommand needs an
    /// authenticated client, which is resolved **once** here and threaded
    /// down so each leaf takes `&GmailClient` and stays free of process
    /// env.
    pub async fn execute(self) -> Result<()> {
        // Propagates --account to the env var `gmail::account::resolve_account`
        // reads (issue #1500), mirroring `Cli::propagate_global_flags`'s
        // pattern: only set when present, so an existing ambient
        // OMNI_DEV_GMAIL_ACCOUNT still works when the flag is omitted.
        if let Some(account) = &self.account {
            std::env::set_var(GMAIL_ACCOUNT_ENV, account);
        }

        match self.command {
            GmailSubcommands::Auth(cmd) => cmd.execute().await,
            GmailSubcommands::Account(cmd) => cmd.execute(),
            data => {
                let client = helpers::create_client()?;
                data.dispatch(&client).await
            }
        }
    }
}

impl GmailSubcommands {
    /// Routes a non-`Auth`/`Account` subcommand against the shared client.
    /// Kept separate from credential resolution so it is testable without
    /// env (tests pass a client pointed at an unreachable URL). The `Auth`
    /// and `Account` arms are unreachable because both are handled before
    /// client resolution in [`GmailCommand::execute`].
    async fn dispatch(self, client: &GmailClient) -> Result<()> {
        match self {
            Self::Auth(_) => {
                unreachable!("Auth is dispatched before client resolution")
            }
            Self::Account(_) => {
                unreachable!("Account is dispatched before client resolution")
            }
            Self::Search(cmd) => cmd.execute(client).await,
            Self::Read(cmd) => cmd.execute(client).await,
            Self::Thread(cmd) => cmd.execute(client).await,
            Self::Label(cmd) => cmd.execute(client).await,
            Self::Sync(cmd) => cmd.execute(client).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::gmail::format::OutputFormat;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::gmail::client::GmailClient;
    use crate::utils::secret::Secret;

    fn dead_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: GmailScope::ReadOnly,
        }
    }

    /// A client pointed at an unreachable URL. Routing tests use it so a
    /// command runs through dispatch -> leaf -> the HTTP layer and fails
    /// with a connection error — exercising the routing without touching
    /// credentials, the process environment, or a mock server.
    fn dead_client() -> GmailClient {
        GmailClient::new("http://127.0.0.1:1", &dead_credentials()).unwrap()
    }

    // ── GmailCommand::execute glue ──────────────────────────────────
    //
    // The success path (`helpers::create_client()` succeeding, then
    // `data.dispatch(&client)` actually issuing a request) needs real
    // credentials and would hit the real Gmail API host — there's no
    // env-var override to redirect it at a mock server (see the
    // `mcp::gmail_tools` module doc). These tests cover the
    // credentials-missing error path, which is deterministic and
    // network-free.

    #[tokio::test]
    async fn execute_routes_auth_subcommand_and_surfaces_missing_credentials() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = GmailCommand {
            account: None,
            command: GmailSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand { all: false }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn execute_non_auth_subcommand_errors_when_credentials_missing() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = GmailCommand {
            account: None,
            command: GmailSubcommands::Search(search::SearchCommand {
                query: "label:finance".to_string(),
                limit: 10,
                enrich: false,
                concurrency: 4,
                output: OutputFormat::Table,
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn gmail_subcommands_auth_variant() {
        let cmd = GmailCommand {
            account: None,
            command: GmailSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand { all: false }),
            }),
        };
        assert!(matches!(cmd.command, GmailSubcommands::Auth(_)));
    }

    #[test]
    fn gmail_subcommands_account_variant() {
        let cmd = GmailCommand {
            account: None,
            command: GmailSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, GmailSubcommands::Account(_)));
    }

    #[tokio::test]
    async fn execute_propagates_account_flag_to_env_var() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = GmailCommand {
            account: Some("work".to_string()),
            command: GmailSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        cmd.execute().await.unwrap();
        assert_eq!(
            std::env::var(GMAIL_ACCOUNT_ENV).ok().as_deref(),
            Some("work")
        );
    }

    #[tokio::test]
    async fn execute_absent_account_leaves_ambient_env_var_untouched() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        std::env::set_var(GMAIL_ACCOUNT_ENV, "personal");

        let cmd = GmailCommand {
            account: None,
            command: GmailSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        cmd.execute().await.unwrap();
        assert_eq!(
            std::env::var(GMAIL_ACCOUNT_ENV).ok().as_deref(),
            Some("personal")
        );
    }

    #[tokio::test]
    async fn execute_routes_account_list_without_client_resolution() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = GmailCommand {
            account: None,
            command: GmailSubcommands::Account(account::AccountCommand {
                command: account::AccountSubcommands::List(account::list::ListCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        // Succeeds even with zero credentials configured — proving Account
        // subcommands never resolve a client (unlike every other
        // subcommand, which errors on missing credentials).
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_routes_search() {
        let cmd = GmailSubcommands::Search(search::SearchCommand {
            query: "label:finance".to_string(),
            limit: 10,
            enrich: false,
            concurrency: 4,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_read() {
        let cmd = GmailSubcommands::Read(read::ReadCommand {
            message_id: "msg1".to_string(),
            out_file: None,
            detail: read::ReadDetail::Full,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_thread() {
        let cmd = GmailSubcommands::Thread(thread::ThreadCommand {
            thread_id: "t1".to_string(),
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_label_list() {
        let cmd = GmailSubcommands::Label(label::LabelCommand {
            command: label::LabelSubcommands::List(label::list::ListCommand {
                output: OutputFormat::Table,
            }),
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_label_add() {
        let cmd = GmailSubcommands::Label(label::LabelCommand {
            command: label::LabelSubcommands::Add(label::add::AddCommand {
                message_ids: vec!["m1".to_string()],
                label: "IMPORTANT".to_string(),
            }),
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_label_remove() {
        let cmd = GmailSubcommands::Label(label::LabelCommand {
            command: label::LabelSubcommands::Remove(label::remove::RemoveCommand {
                message_ids: vec!["m1".to_string()],
                label: "IMPORTANT".to_string(),
                force: true,
                dry_run: false,
            }),
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_sync() {
        let cmd = GmailSubcommands::Sync(sync::SyncCommand {
            output_dir: std::path::PathBuf::from("/tmp/does-not-matter"),
            query: None,
            full: false,
            concurrency: 4,
            dry_run: false,
            extract_attachments: false,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }
}
