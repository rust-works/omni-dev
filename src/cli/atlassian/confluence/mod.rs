//! Confluence CLI subcommands.

pub(crate) mod children;
pub(crate) mod comment;
pub(crate) mod create;
pub(crate) mod delete;
pub(crate) mod download;
pub(crate) mod edit;
pub(crate) mod label;
pub(crate) mod read;
pub(crate) mod search;
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
    /// Manages labels on Confluence pages.
    Label(label::LabelCommand),
    /// Recursively downloads a Confluence page tree.
    Download(download::DownloadCommand),
    /// Lists child pages of a Confluence page or top-level pages in a space.
    Children(children::ChildrenCommand),
    /// Confluence user operations.
    User(user::UserCommand),
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
            ConfluenceSubcommands::Delete(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Download(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Children(cmd) => cmd.execute().await,
            ConfluenceSubcommands::User(cmd) => cmd.execute().await,
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
    fn confluence_subcommands_delete_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Delete(delete::DeleteCommand {
                id: "12345".to_string(),
                force: true,
                purge: false,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Delete(_)));
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

    /// Exercises the `Children` dispatch arm in `ConfluenceCommand::execute`
    /// with injected fake credentials so `create_client()` succeeds and the
    /// downstream call is reached. The subsequent API call is allowed to
    /// fail — we only care that the dispatch line runs.
    #[tokio::test]
    async fn confluence_command_execute_children_dispatch() {
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

    #[test]
    fn confluence_subcommands_download_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Download(download::DownloadCommand {
                id: "12345".to_string(),
                output_dir: std::path::PathBuf::from("."),
                format: ContentFormat::Jfm,
                concurrency: 8,
                max_depth: 0,
                resume: false,
                on_conflict: download::OnConflict::Backup,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Download(_)));
    }
}
