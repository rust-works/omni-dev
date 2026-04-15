//! JIRA CLI subcommands.

pub(crate) mod attachment;
pub(crate) mod board;
pub(crate) mod changelog;
pub(crate) mod comment;
pub(crate) mod create;
pub(crate) mod delete;
pub(crate) mod dev;
pub(crate) mod edit;
pub(crate) mod field;
pub(crate) mod link;
pub(crate) mod project;
pub(crate) mod read;
pub(crate) mod search;
pub(crate) mod sprint;
pub(crate) mod transition;
pub(crate) mod write;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// JIRA issue management, search, agile boards, and more.
#[derive(Parser)]
pub struct JiraCommand {
    /// The JIRA subcommand to execute.
    #[command(subcommand)]
    pub command: JiraSubcommands,
}

/// JIRA subcommands.
#[derive(Subcommand)]
pub enum JiraSubcommands {
    /// Fetches a JIRA issue and outputs it as JFM markdown or ADF JSON.
    Read(read::ReadCommand),
    /// Pushes content to a JIRA issue.
    Write(write::WriteCommand),
    /// Interactive fetch-edit-push cycle for a JIRA issue.
    Edit(edit::EditCommand),
    /// Searches JIRA issues using JQL.
    Search(search::SearchCommand),
    /// Creates a new JIRA issue.
    Create(create::CreateCommand),
    /// Lists or executes workflow transitions on a JIRA issue.
    Transition(transition::TransitionCommand),
    /// Manages comments on a JIRA issue.
    Comment(comment::CommentCommand),
    /// Deletes a JIRA issue.
    Delete(delete::DeleteCommand),
    /// Shows development status (linked PRs, branches, repositories) for a JIRA issue.
    Dev(dev::DevCommand),
    /// Lists JIRA projects.
    Project(project::ProjectCommand),
    /// Manages JIRA field definitions and options.
    Field(field::FieldCommand),
    /// Manages JIRA agile boards.
    Board(board::BoardCommand),
    /// Manages JIRA agile sprints.
    Sprint(sprint::SprintCommand),
    /// Manages JIRA issue links.
    Link(link::LinkCommand),
    /// Shows change history for JIRA issues.
    Changelog(changelog::ChangelogCommand),
    /// Downloads JIRA issue attachments.
    Attachment(attachment::AttachmentCommand),
}

impl JiraCommand {
    /// Executes the JIRA command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            JiraSubcommands::Read(cmd) => cmd.execute().await,
            JiraSubcommands::Write(cmd) => cmd.execute().await,
            JiraSubcommands::Edit(cmd) => cmd.execute().await,
            JiraSubcommands::Search(cmd) => cmd.execute().await,
            JiraSubcommands::Create(cmd) => cmd.execute().await,
            JiraSubcommands::Transition(cmd) => cmd.execute().await,
            JiraSubcommands::Comment(cmd) => cmd.execute().await,
            JiraSubcommands::Delete(cmd) => cmd.execute().await,
            JiraSubcommands::Dev(cmd) => cmd.execute().await,
            JiraSubcommands::Project(cmd) => cmd.execute().await,
            JiraSubcommands::Field(cmd) => cmd.execute().await,
            JiraSubcommands::Board(cmd) => cmd.execute().await,
            JiraSubcommands::Sprint(cmd) => cmd.execute().await,
            JiraSubcommands::Link(cmd) => cmd.execute().await,
            JiraSubcommands::Changelog(cmd) => cmd.execute().await,
            JiraSubcommands::Attachment(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::atlassian::format::{ContentFormat, OutputFormat};

    #[test]
    fn jira_subcommands_read_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Read(read::ReadCommand {
                key: "PROJ-1".to_string(),
                output: None,
                format: ContentFormat::Jfm,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Read(_)));
    }

    #[test]
    fn jira_subcommands_write_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Write(write::WriteCommand {
                key: "PROJ-1".to_string(),
                file: None,
                format: ContentFormat::Jfm,
                force: false,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Write(_)));
    }

    #[test]
    fn jira_subcommands_edit_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Edit(edit::EditCommand {
                key: "PROJ-1".to_string(),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Edit(_)));
    }

    #[test]
    fn jira_subcommands_create_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Create(create::CreateCommand {
                file: None,
                format: ContentFormat::Jfm,
                project: Some("PROJ".to_string()),
                r#type: None,
                summary: Some("Test".to_string()),
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Create(_)));
    }

    #[test]
    fn jira_subcommands_search_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Search(search::SearchCommand {
                jql: Some("project = PROJ".to_string()),
                project: None,
                assignee: None,
                status: None,
                limit: 50,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Search(_)));
    }

    #[test]
    fn jira_subcommands_transition_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Transition(transition::TransitionCommand {
                key: "PROJ-1".to_string(),
                transition: Some("Done".to_string()),
                list: false,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Transition(_)));
    }

    #[test]
    fn jira_subcommands_comment_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Comment(comment::CommentCommand {
                command: comment::CommentSubcommands::List(comment::ListCommand {
                    key: "PROJ-1".to_string(),
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Comment(_)));
    }

    #[test]
    fn jira_subcommands_delete_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Delete(delete::DeleteCommand {
                key: "PROJ-1".to_string(),
                force: true,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Delete(_)));
    }

    #[test]
    fn jira_subcommands_dev_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Dev(dev::DevCommand {
                key: "PROJ-1".to_string(),
                r#type: None,
                app: None,
                summary: false,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Dev(_)));
    }

    #[test]
    fn jira_subcommands_project_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Project(project::ProjectCommand {
                command: project::ProjectSubcommands::List(project::ListCommand {
                    limit: 50,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Project(_)));
    }

    #[test]
    fn jira_subcommands_field_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Field(field::FieldCommand {
                command: field::FieldSubcommands::List(field::ListCommand {
                    search: None,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Field(_)));
    }

    #[test]
    fn jira_subcommands_board_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Board(board::BoardCommand {
                command: board::BoardSubcommands::List(board::ListCommand {
                    project: None,
                    r#type: None,
                    limit: 50,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Board(_)));
    }

    #[test]
    fn jira_subcommands_sprint_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Sprint(sprint::SprintCommand {
                command: sprint::SprintSubcommands::List(sprint::ListCommand {
                    board_id: 1,
                    state: None,
                    limit: 50,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Sprint(_)));
    }

    #[test]
    fn jira_subcommands_link_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Link(link::LinkCommand {
                command: link::LinkSubcommands::Types(link::TypesCommand {
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Link(_)));
    }

    #[test]
    fn jira_subcommands_changelog_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Changelog(changelog::ChangelogCommand {
                keys: "PROJ-1".to_string(),
                limit: 50,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Changelog(_)));
    }

    #[test]
    fn jira_subcommands_attachment_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Attachment(attachment::AttachmentCommand {
                command: attachment::AttachmentSubcommands::Download(attachment::DownloadCommand {
                    key: "PROJ-1".to_string(),
                    output_dir: ".".to_string(),
                    filter: None,
                }),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Attachment(_)));
    }
}
