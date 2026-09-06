//! Drive CLI commands.

pub(crate) mod account;
pub(crate) mod auth;
pub(crate) mod create;
pub(crate) mod dedupe;
pub(crate) mod edit;
pub(crate) mod format;
pub(crate) mod helpers;
/// `drive move` — named `move_file` (not `move`, a Rust keyword) mirroring
/// `crate::cli::atlassian::confluence::move_page`'s identical workaround.
pub(crate) mod move_file;
pub(crate) mod permissions;
pub(crate) mod read;
pub(crate) mod rename;
pub(crate) mod search;
pub(crate) mod sheets;
pub(crate) mod upload;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::drive::account::DRIVE_ACCOUNT_ENV;
use crate::drive::client::DriveClient;

/// Drive: search, read, rename, and move Google Drive files via OAuth2.
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
    /// Creates a new file or folder, gated by the folder write-permission
    /// rules (issue #1574). Requires the `drive.file` or `drive` scope
    /// (`drive auth login --write-file`/`--write-full`).
    Create(create::CreateCommand),
    /// Uploads local content as a new file, gated by the folder
    /// write-permission rules (issue #1574). Requires the `drive.file` or
    /// `drive` scope (`drive auth login --write-file`/`--write-full`).
    Upload(upload::UploadCommand),
    /// Replaces an existing file's content, gated by the
    /// write-permission rules (issues #1574, #1612). Requires the `drive.file`
    /// scope if `omni-dev` created the file, or the unrestricted `drive`
    /// scope for any pre-existing file (`drive auth login --write-file`
    /// or `--write-full`).
    Edit(edit::EditCommand),
    /// Renames a single Drive file. Requires the `drive.metadata` scope
    /// (`drive auth login --write`).
    Rename(rename::RenameCommand),
    /// Moves one or more Drive files into a destination folder. Requires
    /// the `drive.metadata` scope (`drive auth login --write`).
    Move(move_file::MoveCommand),
    /// Inspects the write-permission rules gating `drive
    /// create`/`upload`/`edit` and `drive sheets
    /// write`/`append`/`clear`/`create` (issues #1574, #1589, #1612).
    Permissions(permissions::PermissionsCommand),
    /// Reads and writes the cells of a Google Sheet via the Sheets v4 API
    /// (issue #1589).
    Sheets(sheets::SheetsCommand),
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
            // Permissions' three leaves have mixed client needs (`show` is
            // config-only, `lookup-folder`/`check` both call the Drive
            // API) — like Auth, it resolves its own client lazily per leaf
            // rather than sharing the single eager resolution below.
            DriveSubcommands::Permissions(cmd) => cmd.execute().await,
            command => {
                let client = helpers::create_client()?;
                command.dispatch(&client).await
            }
        }
    }
}

impl DriveSubcommands {
    /// Routes a non-`Auth`/`Account`/`Permissions` subcommand against the
    /// shared client. Those three arms are unreachable: all are handled
    /// before client resolution in [`DriveCommand::execute`].
    async fn dispatch(self, client: &DriveClient) -> Result<()> {
        match self {
            Self::Auth(_) => unreachable!("Auth is dispatched before client resolution"),
            Self::Account(_) => unreachable!("Account is dispatched before client resolution"),
            Self::Permissions(_) => {
                unreachable!("Permissions is dispatched before client resolution")
            }
            Self::Search(cmd) => cmd.execute(client).await,
            Self::Read(cmd) => cmd.execute(client).await,
            Self::Dedupe(cmd) => cmd.execute(client).await,
            Self::Create(cmd) => cmd.execute(client).await,
            Self::Upload(cmd) => cmd.execute(client).await,
            Self::Edit(cmd) => cmd.execute(client).await,
            Self::Rename(cmd) => cmd.execute(client).await,
            Self::Move(cmd) => cmd.execute(client).await,
            Self::Sheets(cmd) => cmd.execute(client).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::drive::format::OutputFormat;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    fn dead_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: DriveGrantedScopes::READONLY,
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
    async fn execute_routes_permissions_show_without_client_resolution() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DriveCommand {
            account: None,
            command: DriveSubcommands::Permissions(permissions::PermissionsCommand {
                command: permissions::PermissionsSubcommands::Show(
                    permissions::show::ShowCommand {
                        output: OutputFormat::Table,
                    },
                ),
            }),
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn execute_routes_permissions_check_and_surfaces_missing_credentials() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DriveCommand {
            account: None,
            command: DriveSubcommands::Permissions(permissions::PermissionsCommand {
                command: permissions::PermissionsSubcommands::Check(
                    permissions::check::CheckCommand {
                        id: "f1".to_string(),
                        operation: permissions::check::OperationArg::Read,
                        output: OutputFormat::Table,
                    },
                ),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn dispatch_routes_sheets_info() {
        // Unlike every other routing test, this one needs an env guard: a
        // `SheetsClient` with no `SHEETS_API_URL` set resolves the *real*
        // `sheets.googleapis.com`, so without the redirect this test would
        // send a request to Google.
        let guard = crate::drive::test_support::EnvGuard::take();
        guard.redirect_api_hosts_to_a_dead_port();

        let cmd = DriveSubcommands::Sheets(sheets::SheetsCommand {
            command: sheets::SheetsSubcommands::Info(sheets::info::InfoCommand {
                spreadsheet_id: "s1".to_string(),
                output: OutputFormat::Table,
            }),
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_sheets_read() {
        let guard = crate::drive::test_support::EnvGuard::take();
        guard.redirect_api_hosts_to_a_dead_port();

        let cmd = DriveSubcommands::Sheets(sheets::SheetsCommand {
            command: sheets::SheetsSubcommands::Read(sheets::read::ReadCommand {
                spreadsheet_id: "s1".to_string(),
                range: None,
                sheet: None,
                render: sheets::read::RenderArg::Formatted,
                output: OutputFormat::Table,
            }),
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_sheets_write() {
        let guard = crate::drive::test_support::EnvGuard::take();
        guard.redirect_api_hosts_to_a_dead_port();
        let _dir = guard.clear_credentials();

        let cmd = DriveSubcommands::Sheets(sheets::SheetsCommand {
            command: sheets::SheetsSubcommands::Write(sheets::write::WriteCommand {
                spreadsheet_id: "s1".to_string(),
                range: Some("A1".to_string()),
                sheet: None,
                values: "/definitely/not/here.csv".to_string(),
                values_format: sheets::values::ValuesFormat::Auto,
                input: sheets::write::InputArg::UserEntered,
                dry_run: false,
                output: OutputFormat::Table,
            }),
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_sheets_append() {
        let guard = crate::drive::test_support::EnvGuard::take();
        guard.redirect_api_hosts_to_a_dead_port();
        let _dir = guard.clear_credentials();

        let cmd = DriveSubcommands::Sheets(sheets::SheetsCommand {
            command: sheets::SheetsSubcommands::Append(sheets::write::AppendCommand {
                spreadsheet_id: "s1".to_string(),
                range: Some("A1".to_string()),
                sheet: None,
                values: "/definitely/not/here.csv".to_string(),
                values_format: sheets::values::ValuesFormat::Auto,
                input: sheets::write::InputArg::UserEntered,
                dry_run: false,
                output: OutputFormat::Table,
            }),
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_sheets_clear() {
        let guard = crate::drive::test_support::EnvGuard::take();
        guard.redirect_api_hosts_to_a_dead_port();
        let _dir = guard.clear_credentials();

        let cmd = DriveSubcommands::Sheets(sheets::SheetsCommand {
            command: sheets::SheetsSubcommands::Clear(sheets::write::ClearCommand {
                spreadsheet_id: "s1".to_string(),
                range: Some("A1".to_string()),
                sheet: None,
                dry_run: false,
                output: OutputFormat::Table,
            }),
        });
        // Unlike its siblings this returns `Ok`, and that is the contract,
        // not an accident: `write`/`append` fail here only because reading
        // `--values` fails *before* the engine. `clear` reaches the engine,
        // which never returns `Err` — every failure is a `WriteResult`
        // variant and the process still exits 0 (ADR-0070 §10, ADR-0071
        // §12). Scripts must inspect the output, not `$?`.
        assert!(cmd.dispatch(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_routes_sheets_create() {
        let guard = crate::drive::test_support::EnvGuard::take();
        guard.redirect_api_hosts_to_a_dead_port();
        let _dir = guard.clear_credentials();

        let cmd = DriveSubcommands::Sheets(sheets::SheetsCommand {
            command: sheets::SheetsSubcommands::Create(sheets::create::CreateCommand {
                name: "Budget".to_string(),
                parent: "parent-1".to_string(),
                values: None,
                values_format: sheets::values::ValuesFormat::Auto,
                input: sheets::write::InputArg::UserEntered,
                dry_run: false,
                output: OutputFormat::Table,
            }),
        });
        // Like `clear`, this reaches the engine (no `--values` to fail
        // reading first), and `create` never returns `Err` either — every
        // failure is a `CreateResult` variant (ADR-0070 §10, ADR-0071 §12).
        assert!(cmd.dispatch(&dead_client()).await.is_ok());
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

    #[tokio::test]
    async fn dispatch_routes_create() {
        // Unlike rename/move (whose engine fns return `Result` and
        // propagate a network error via `?`), `create`'s engine fn always
        // returns an `Ok`-shaped `CreateOutcome` — a fetch failure against
        // the dead client becomes an embedded `Failed{detail}`, matching
        // the exit-0-regardless-of-outcome convention (ADR-0071 §12), not
        // a dispatch-level error. Needs env isolation (unlike the other
        // dispatch_routes_* tests): `create`'s CLI layer resolves the
        // active account's write_permissions.rules via `Settings::load()`,
        // so without a clean $HOME it reads whatever real
        // ~/.omni-dev/settings.json this process has.
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DriveSubcommands::Create(create::CreateCommand {
            name: "New File".to_string(),
            parent: "parent-1".to_string(),
            folder: false,
            mime_type: None,
            dry_run: false,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_routes_upload() {
        // Same env-isolation and exit-0-regardless-of-outcome reasoning as
        // dispatch_routes_create.
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        let content_dir = tempfile::tempdir().unwrap();
        let local_path = content_dir.path().join("upload-me.txt");
        std::fs::write(&local_path, b"content").unwrap();

        let cmd = DriveSubcommands::Upload(upload::UploadCommand {
            local_path,
            parent: "parent-1".to_string(),
            name: None,
            mime_type: None,
            dry_run: false,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_routes_edit() {
        // Same env-isolation and exit-0-regardless-of-outcome reasoning as
        // dispatch_routes_create/dispatch_routes_upload.
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        let content_dir = tempfile::tempdir().unwrap();
        let content_path = content_dir.path().join("new-content.txt");
        std::fs::write(&content_path, b"content").unwrap();

        let cmd = DriveSubcommands::Edit(edit::EditCommand {
            file_id: "f1".to_string(),
            content: content_path.to_str().unwrap().to_string(),
            mime_type: None,
            dry_run: false,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_routes_rename() {
        let cmd = DriveSubcommands::Rename(rename::RenameCommand {
            file_id: "f1".to_string(),
            new_name: "New Name".to_string(),
            dry_run: false,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_move() {
        let cmd = DriveSubcommands::Move(move_file::MoveCommand {
            file_ids: vec!["f1".to_string()],
            to: "dest1".to_string(),
            allow_visibility_increase: false,
            allow_visibility_decrease: false,
            allow_drive_boundary_crossing: false,
            dry_run: false,
            output: OutputFormat::Table,
        });
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }
}
