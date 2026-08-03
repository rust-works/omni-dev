//! Gmail CLI commands.

pub(crate) mod auth;
pub(crate) mod format;
pub(crate) mod helpers;
pub(crate) mod label;
pub(crate) mod read;
pub(crate) mod search;
pub(crate) mod thread;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::gmail::client::GmailClient;

/// Gmail: read Gmail messages, threads, and labels (and, with `gmail.modify`, mutate labels).
#[derive(Parser)]
pub struct GmailCommand {
    /// The Gmail subcommand to execute.
    #[command(subcommand)]
    pub command: GmailSubcommands,
}

/// Gmail subcommands.
#[derive(Subcommand)]
pub enum GmailSubcommands {
    /// Manages Gmail OAuth2 credentials (mirrors the `gmail_auth_status` MCP tool for `status`).
    Auth(auth::AuthCommand),
    /// Searches Gmail messages (mirrors the `gmail_search` MCP tool).
    Search(search::SearchCommand),
    /// Reads a single Gmail message (mirrors the `gmail_message_read` MCP tool).
    Read(read::ReadCommand),
    /// Reads a Gmail thread (mirrors the `gmail_thread_read` MCP tool).
    Thread(thread::ThreadCommand),
    /// Manages Gmail labels (mirrors the `gmail_label_list` MCP tool; `add`/`remove` are CLI-only in Phase 1).
    Label(label::LabelCommand),
}

impl GmailCommand {
    /// Executes the Gmail command.
    ///
    /// `auth` manages credentials and must run without them; every other
    /// subcommand needs an authenticated client, which is resolved **once**
    /// here and threaded down so each leaf takes `&GmailClient` and stays
    /// free of process env.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            GmailSubcommands::Auth(cmd) => cmd.execute().await,
            data => {
                let client = helpers::create_client()?;
                data.dispatch(&client).await
            }
        }
    }
}

impl GmailSubcommands {
    /// Routes a non-`Auth` subcommand against the shared client. Kept separate
    /// from credential resolution so it is testable without env (tests pass a
    /// client pointed at an unreachable URL). The `Auth` arm is unreachable
    /// because it is handled before client resolution in
    /// [`GmailCommand::execute`].
    async fn dispatch(self, client: &GmailClient) -> Result<()> {
        match self {
            Self::Auth(_) => {
                unreachable!("Auth is dispatched before client resolution")
            }
            Self::Search(cmd) => cmd.execute(client).await,
            Self::Read(cmd) => cmd.execute(client).await,
            Self::Thread(cmd) => cmd.execute(client).await,
            Self::Label(cmd) => cmd.execute(client).await,
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
            command: GmailSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand),
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
            command: GmailSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand),
            }),
        };
        assert!(matches!(cmd.command, GmailSubcommands::Auth(_)));
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
}
