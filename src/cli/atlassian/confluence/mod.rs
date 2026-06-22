//! Confluence CLI subcommands.

pub(crate) mod attachment;
pub(crate) mod children;
pub(crate) mod comment;
pub(crate) mod compare;
pub(crate) mod create;
pub(crate) mod delete;
pub(crate) mod download;
pub(crate) mod edit;
pub(crate) mod history;
pub(crate) mod label;
pub(crate) mod move_page;
pub(crate) mod read;
pub(crate) mod search;
pub(crate) mod space;
pub(crate) mod user;
pub(crate) mod write;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Confluence page management, search, and more.
#[derive(Parser)]
pub struct ConfluenceCommand {
    /// The Confluence subcommand to execute.
    #[command(subcommand)]
    pub command: ConfluenceSubcommands,
}

/// Confluence subcommands.
#[derive(Subcommand)]
pub enum ConfluenceSubcommands {
    /// Manages comments on a Confluence page.
    Comment(comment::CommentCommand),
    /// Fetches a Confluence page and outputs it as JFM markdown or ADF JSON.
    Read(read::ReadCommand),
    /// Pushes content to a Confluence page.
    Write(write::WriteCommand),
    /// Interactive fetch-edit-push cycle for a Confluence page.
    Edit(edit::EditCommand),
    /// Searches Confluence pages using CQL.
    Search(search::SearchCommand),
    /// Creates a new Confluence page.
    Create(create::CreateCommand),
    /// Deletes a Confluence page.
    Delete(delete::DeleteCommand),
    /// Moves or reparents a Confluence page (same-space only).
    Move(move_page::MoveCommand),
    /// Manages labels on Confluence pages.
    Label(label::LabelCommand),
    /// Manages attachments on Confluence pages.
    Attachment(attachment::AttachmentCommand),
    /// Recursively downloads a Confluence page tree.
    Download(download::DownloadCommand),
    /// Lists child pages of a Confluence page or top-level pages in a space.
    Children(children::ChildrenCommand),
    /// Lists version history (metadata) for a Confluence page.
    History(history::HistoryCommand),
    /// Compares two versions of a Confluence page (structural diff).
    Compare(compare::CompareCommandGroup),
    /// Confluence user operations.
    User(user::UserCommand),
    /// Confluence space operations.
    Space(space::SpaceCommand),
}

impl ConfluenceCommand {
    /// Executes the Confluence command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            ConfluenceSubcommands::Comment(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Read(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Write(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Edit(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Search(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Create(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Label(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Attachment(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Delete(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Move(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Download(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Children(cmd) => cmd.execute().await,
            ConfluenceSubcommands::History(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Compare(cmd) => cmd.execute().await,
            ConfluenceSubcommands::User(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Space(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::atlassian::format::{ContentFormat, OutputFormat};

    #[test]
    fn confluence_subcommands_comment_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Comment(comment::CommentCommand {
                command: comment::CommentSubcommands::List(comment::ListCommand {
                    id: "12345".to_string(),
                    kind: comment::CommentKindFilter::All,
                    limit: 25,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Comment(_)));
    }

    #[test]
    fn confluence_subcommands_read_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Read(read::ReadCommand {
                id: "12345".to_string(),
                output: None,
                format: ContentFormat::Jfm,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Read(_)));
    }

    #[test]
    fn confluence_subcommands_write_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Write(write::WriteCommand {
                id: "12345".to_string(),
                file: None,
                format: ContentFormat::Adf,
                force: false,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Write(_)));
    }

    #[test]
    fn confluence_subcommands_edit_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Edit(edit::EditCommand {
                id: "12345".to_string(),
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Edit(_)));
    }

    #[test]
    fn confluence_subcommands_search_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Search(search::SearchCommand {
                cql: Some("space = ENG".to_string()),
                space: None,
                title: None,
                limit: 25,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Search(_)));
    }

    #[test]
    fn confluence_subcommands_create_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Create(create::CreateCommand {
                file: None,
                format: ContentFormat::Jfm,
                space: Some("ENG".to_string()),
                title: Some("Test".to_string()),
                parent: None,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Create(_)));
    }

    #[test]
    fn confluence_subcommands_label_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Label(label::LabelCommand {
                command: label::LabelSubcommands::List(label::ListCommand {
                    id: "12345".to_string(),
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Label(_)));
    }

    #[test]
    fn confluence_subcommands_attachment_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Attachment(attachment::AttachmentCommand {
                command: attachment::AttachmentSubcommands::List(attachment::ListCommand {
                    page_id: "12345".to_string(),
                    cursor: None,
                    limit: 25,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Attachment(_)));
    }

    #[test]
    fn confluence_subcommands_delete_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Delete(delete::DeleteCommand {
                id: "12345".to_string(),
                force: true,
                dry_run: false,
                purge: false,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Delete(_)));
    }

    #[test]
    fn confluence_subcommands_move_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Move(move_page::MoveCommand {
                id: "12345".to_string(),
                target: "456".to_string(),
                position: move_page::MovePosition::Append,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Move(_)));
    }

    /// Exercises the `Move` dispatch arm in `ConfluenceCommand::execute` with
    /// injected fake credentials so `create_client()` succeeds; the API call
    /// is allowed to fail.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn confluence_command_execute_move_dispatch() {
        // Serialise on the one canonical env mutex (issue #950).
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Move(move_page::MoveCommand {
                id: "12345".to_string(),
                target: "456".to_string(),
                position: move_page::MovePosition::Append,
            }),
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[test]
    fn confluence_subcommands_user_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::User(user::UserCommand {
                command: user::UserSubcommands::Search(user::UserSearchCommand {
                    query: "alice".to_string(),
                    limit: 25,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::User(_)));
    }

    #[test]
    fn confluence_subcommands_space_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Space(space::SpaceCommand {
                command: space::SpaceSubcommands::List(space::ListCommand {
                    keys: vec![],
                    r#type: None,
                    status: None,
                    cursor: None,
                    limit: 25,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Space(_)));
    }

    #[test]
    fn confluence_subcommands_space_pages_variant() {
        let pages = ConfluenceCommand {
            command: ConfluenceSubcommands::Space(space::SpaceCommand {
                command: space::SpaceSubcommands::Pages(space::PagesCommand {
                    key: "ENG".to_string(),
                    status: None,
                    sort: None,
                    cursor: None,
                    limit: 25,
                    output: OutputFormat::Table,
                }),
            }),
        };
        let other = ConfluenceCommand {
            command: ConfluenceSubcommands::Read(read::ReadCommand {
                id: "1".to_string(),
                output: None,
                format: ContentFormat::Jfm,
            }),
        };
        // Single `matches!` site exercised against both a matching and
        // non-matching variant so both arms are covered at the same source
        // line (avoids the partial-branch noise of two separate sites).
        for (expected, cmd) in [(true, pages), (false, other)] {
            assert_eq!(
                matches!(cmd.command, ConfluenceSubcommands::Space(_)),
                expected
            );
        }
    }

    /// Exercises the `Space` dispatch arm in `ConfluenceCommand::execute`
    /// with injected fake credentials so `create_client()` succeeds and the
    /// downstream call is reached. The subsequent API call is allowed to
    /// fail — we only care that the dispatch line runs.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn confluence_command_execute_space_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Space(space::SpaceCommand {
                command: space::SpaceSubcommands::List(space::ListCommand {
                    keys: vec![],
                    r#type: None,
                    status: None,
                    cursor: None,
                    limit: 25,
                    output: OutputFormat::Table,
                }),
            }),
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[test]
    fn confluence_subcommands_children_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Children(children::ChildrenCommand {
                id: Some("12345".to_string()),
                space: None,
                recursive: false,
                max_depth: 0,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Children(_)));
    }

    #[test]
    fn confluence_subcommands_history_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::History(history::HistoryCommand {
                id: "12345".to_string(),
                since: None,
                limit: 20,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::History(_)));
    }

    /// Exercises the `History` dispatch arm in `ConfluenceCommand::execute`
    /// with injected fake credentials so `create_client()` succeeds and the
    /// downstream call is reached. The subsequent API call is allowed to
    /// fail — we only care that the dispatch line runs.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn confluence_command_execute_history_dispatch() {
        // Serialise on the one canonical env mutex (issue #950).
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::History(history::HistoryCommand {
                id: "12345".to_string(),
                since: None,
                limit: 20,
                output: OutputFormat::Table,
            }),
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    /// Exercises the `Children` dispatch arm in `ConfluenceCommand::execute`
    /// with injected fake credentials so `create_client()` succeeds and the
    /// downstream call is reached. The subsequent API call is allowed to
    /// fail — we only care that the dispatch line runs.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn confluence_command_execute_children_dispatch() {
        // Routes through the crate-wide `AUTH_ENV_MUTEX` so the env-var
        // mutation doesn't race against other Atlassian-touching tests.
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Children(children::ChildrenCommand {
                id: Some("12345".to_string()),
                space: None,
                recursive: false,
                max_depth: 0,
                output: OutputFormat::Table,
            }),
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    /// Exercises the `Attachment` dispatch arm in `ConfluenceCommand::execute`.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn confluence_command_execute_attachment_dispatch() {
        // Serialise on the one canonical env mutex (issue #950).
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Attachment(attachment::AttachmentCommand {
                command: attachment::AttachmentSubcommands::List(attachment::ListCommand {
                    page_id: "12345".to_string(),
                    cursor: None,
                    limit: 25,
                    output: OutputFormat::Table,
                }),
            }),
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[test]
    fn confluence_subcommands_download_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Download(download::DownloadCommand {
                id: Some("12345".to_string()),
                space: None,
                output_dir: std::path::PathBuf::from("."),
                format: ContentFormat::Jfm,
                concurrency: 8,
                max_depth: 0,
                title_filter: None,
                resume: false,
                include_attachments: false,
                on_conflict: download::OnConflict::Backup,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Download(_)));
    }

    #[test]
    fn confluence_subcommands_download_space_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Download(download::DownloadCommand {
                id: None,
                space: Some("AD".to_string()),
                output_dir: std::path::PathBuf::from("./AD"),
                format: ContentFormat::Jfm,
                concurrency: 8,
                max_depth: 0,
                title_filter: Some("architecture".to_string()),
                resume: false,
                include_attachments: false,
                on_conflict: download::OnConflict::Backup,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Download(_)));
    }

    /// Exercises the `Compare` dispatch arm in `ConfluenceCommand::execute`
    /// with injected fake credentials so `create_client()` succeeds and the
    /// downstream call is reached. The subsequent API call is allowed to
    /// fail — we only care that the dispatch line runs.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn confluence_command_execute_compare_dispatch() {
        // Serialise on the one canonical env mutex (issue #950).
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Compare(compare::CompareCommandGroup {
                command: compare::CompareSubcommands::Run(compare::CompareCommand {
                    id: "12345".to_string(),
                    from: "previous".to_string(),
                    to: "latest".to_string(),
                    detail: compare::DetailArg::Outline,
                    include: "body".to_string(),
                    ignore_whitespace: true,
                    min_change_chars: 0,
                    filter_sections: Vec::new(),
                    budget: 16384,
                    output: OutputFormat::Yaml,
                }),
            }),
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }
}
