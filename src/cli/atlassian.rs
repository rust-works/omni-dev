//! Atlassian CLI commands for JIRA and Confluence.

pub(crate) mod auth;
pub mod confluence;
pub(crate) mod convert;
pub(crate) mod format;
pub(crate) mod helpers;
pub mod jira;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

/// `--instance`: per-invocation Atlassian tenant override.
///
/// Scoped to the commands that read Atlassian credentials — subtree-global on
/// `jira` and `confluence`, and a plain flag on `auth status` — per the
/// placement rule in [`crate::cli::ai_backend_args`]. `convert`, `auth login`,
/// and `auth logout` never load credentials, so they reject it (#1778).
#[derive(Args, Debug, Clone, Default)]
pub struct InstanceArg {
    /// Overrides the Atlassian instance URL (e.g.
    /// `https://org.atlassian.net`) for this JIRA/Confluence command.
    ///
    /// Takes precedence over `ATLASSIAN_INSTANCE_URL` / settings.json (email
    /// and API token still come from the environment/settings). Lets a
    /// multi-site user target a specific tenant per invocation. Equivalent to
    /// setting `OMNI_DEV_ATLASSIAN_INSTANCE`.
    #[arg(long, global = true, value_name = "URL")]
    pub instance: Option<String>,
}

impl InstanceArg {
    /// Forwards `--instance` to the env var that
    /// [`crate::atlassian::auth::load_credentials`] reads (#1117). When absent,
    /// any existing value is left untouched so the env-var path still works.
    pub fn apply(&self) {
        if let Some(instance) = &self.instance {
            std::env::set_var(
                crate::atlassian::auth::ATLASSIAN_INSTANCE_OVERRIDE_ENV,
                instance,
            );
        }
    }
}

/// Atlassian: JIRA and Confluence operations.
#[derive(Parser)]
pub struct AtlassianCommand {
    /// The Atlassian subcommand to execute.
    #[command(subcommand)]
    pub command: AtlassianSubcommands,
}

/// Atlassian subcommands.
#[derive(Subcommand)]
pub enum AtlassianSubcommands {
    /// JIRA issue management, search, agile boards, and more.
    Jira(jira::JiraCommand),
    /// Confluence page management, search, and more.
    Confluence(confluence::ConfluenceCommand),
    /// Converts between JFM markdown and ADF JSON.
    Convert(convert::ConvertCommand),
    /// Manages Atlassian Cloud credentials.
    Auth(auth::AuthCommand),
}

impl AtlassianCommand {
    /// Executes the Atlassian command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AtlassianSubcommands::Jira(cmd) => cmd.execute().await,
            AtlassianSubcommands::Confluence(cmd) => cmd.execute().await,
            AtlassianSubcommands::Convert(cmd) => cmd.execute(),
            AtlassianSubcommands::Auth(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── InstanceArg (#1778) ──

    const INSTANCE_VAR: &str = "OMNI_DEV_ATLASSIAN_INSTANCE";

    /// Serialises on the shared env lock and restores `OMNI_DEV_ATLASSIAN_INSTANCE`.
    struct InstanceEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
    }

    impl InstanceEnvGuard {
        fn new() -> Self {
            let lock = crate::claude::ai::claude_cli::CLI_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = std::env::var(INSTANCE_VAR).ok();
            std::env::remove_var(INSTANCE_VAR);
            Self { _lock: lock, saved }
        }
    }

    impl Drop for InstanceEnvGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var(INSTANCE_VAR, v),
                None => std::env::remove_var(INSTANCE_VAR),
            }
        }
    }

    #[test]
    fn instance_apply_sets_env_var() {
        let _g = InstanceEnvGuard::new();
        InstanceArg {
            instance: Some("https://org.atlassian.net".to_string()),
        }
        .apply();
        assert_eq!(
            std::env::var(INSTANCE_VAR).ok().as_deref(),
            Some("https://org.atlassian.net")
        );
    }

    #[test]
    fn instance_apply_absent_leaves_env_var() {
        let _g = InstanceEnvGuard::new();
        std::env::set_var(INSTANCE_VAR, "https://preset.atlassian.net");
        InstanceArg::default().apply();
        assert_eq!(
            std::env::var(INSTANCE_VAR).ok().as_deref(),
            Some("https://preset.atlassian.net")
        );
    }

    #[test]
    fn instance_parses_on_readers_and_is_rejected_elsewhere() {
        use crate::cli::Cli;
        use clap::Parser;

        let url = "https://x.atlassian.net";
        for argv in [
            &[
                "omni-dev",
                "atlassian",
                "jira",
                "--instance",
                url,
                "read",
                "K-1",
            ][..],
            &[
                "omni-dev",
                "atlassian",
                "jira",
                "read",
                "K-1",
                "--instance",
                url,
            ],
            &[
                "omni-dev",
                "atlassian",
                "confluence",
                "--instance",
                url,
                "read",
                "1",
            ],
            &["omni-dev", "atlassian", "auth", "status", "--instance", url],
        ] {
            assert!(Cli::try_parse_from(argv).is_ok(), "{argv:?}");
        }
        for argv in [
            &[
                "omni-dev",
                "--instance",
                url,
                "atlassian",
                "jira",
                "read",
                "K-1",
            ][..],
            &[
                "omni-dev",
                "atlassian",
                "--instance",
                url,
                "jira",
                "read",
                "K-1",
            ],
            &["omni-dev", "atlassian", "auth", "login", "--instance", url],
            &[
                "omni-dev",
                "git",
                "commit",
                "message",
                "view",
                "--instance",
                url,
            ],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn atlassian_subcommands_jira_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Jira(jira::JiraCommand {
                command: jira::JiraSubcommands::Edit(jira::edit::EditCommand {
                    key: "PROJ-1".to_string(),
                }),
                instance: crate::cli::atlassian::InstanceArg::default(),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Jira(_)));
    }

    #[test]
    fn atlassian_subcommands_confluence_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Confluence(confluence::ConfluenceCommand {
                command: confluence::ConfluenceSubcommands::Edit(confluence::edit::EditCommand {
                    id: "12345".to_string(),
                }),
                instance: crate::cli::atlassian::InstanceArg::default(),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Confluence(_)));
    }

    #[test]
    fn atlassian_subcommands_auth_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Login(auth::LoginCommand),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Auth(_)));
    }

    #[test]
    fn atlassian_subcommands_convert_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Convert(convert::ConvertCommand {
                command: convert::ConvertSubcommands::FromAdf(convert::FromAdfCommand {
                    file: None,
                    strip_local_ids: false,
                }),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Convert(_)));
    }
}
