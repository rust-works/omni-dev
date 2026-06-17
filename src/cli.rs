//! CLI interface for omni-dev.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

pub mod ai;
pub mod atlassian;
pub mod browser;
pub mod commands;
pub mod completions;
pub mod config;
pub mod coverage;
pub mod daemon;
pub mod datadog;
pub mod git;
pub mod help;
pub mod resources;
pub mod transcript;

/// CLI-side selector for the AI backend dispatched by
/// [`create_default_claude_client`][crate::claude::client::create_default_claude_client].
///
/// `None` (flag omitted) preserves env-var dispatch; an explicit value
/// overrides `OMNI_DEV_AI_BACKEND`. Propagation to the env var happens
/// in `Cli::propagate_global_flags`.
#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AiBackend {
    /// Default backend dispatch (HTTP to Anthropic/Bedrock/OpenAI/Ollama via
    /// the existing `USE_*` env vars).
    Default,
    /// Shell out to the `claude -p` CLI (reuses an existing Claude Code auth
    /// session). Equivalent to setting `OMNI_DEV_AI_BACKEND=claude-cli`.
    ClaudeCli,
}

/// Top-level clap-derived CLI struct; the library entry point for embedding
/// omni-dev programmatically.
///
/// Global flags (`--ai-backend`, `--claude-cli-allow-tools`,
/// `--claude-cli-allow-mcp`, `--claude-cli-max-budget-usd`, `--models-yaml`)
/// are propagated to environment variables read by downstream factories
/// before dispatching to a [`Commands`] variant.
#[derive(Parser)]
#[command(name = "omni-dev")]
#[command(
    about = "AI-powered git commit rewriter, PR generator, and MCP server for Jira, Confluence, and Datadog.",
    long_about = None
)]
#[command(version)]
pub struct Cli {
    /// Selects the AI backend used by commands that invoke an AI model.
    ///
    /// Overrides the `OMNI_DEV_AI_BACKEND` environment variable.
    #[arg(long, global = true, value_enum)]
    pub ai_backend: Option<AiBackend>,

    /// Weakens the `claude-cli` sandbox by allowing the nested `claude -p`
    /// session to use its default built-in tools (Read, Edit, Write, Bash,
    /// Glob, Grep).
    ///
    /// **Only use for deliberately tool-capable use cases.** By default the
    /// nested session runs with `--tools ""` and cannot touch the
    /// file system. This flag removes that guard. Equivalent to setting
    /// `OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS=true`. Independent of
    /// `--claude-cli-allow-mcp`.
    ///
    /// Ignored when `--ai-backend` is not `claude-cli`.
    #[arg(long, global = true)]
    pub claude_cli_allow_tools: bool,

    /// Weakens the `claude-cli` sandbox by allowing the nested `claude -p`
    /// session to load MCP servers from `~/.claude/settings.json`.
    ///
    /// **Only use deliberately.** MCP servers commonly hold OAuth tokens
    /// (Gmail, Drive, Slack) and may be arbitrary network-attached services;
    /// enabling this exposes them to the nested session. By default the
    /// session runs with `--strict-mcp-config` and no MCP servers load.
    /// Equivalent to setting `OMNI_DEV_CLAUDE_CLI_ALLOW_MCP=true`.
    /// Independent of `--claude-cli-allow-tools`.
    ///
    /// Ignored when `--ai-backend` is not `claude-cli`.
    #[arg(long, global = true)]
    pub claude_cli_allow_mcp: bool,

    /// Per-invocation spending cap in USD for the `claude-cli` backend.
    ///
    /// Forwarded to `claude -p --max-budget-usd`. When the nested session
    /// exceeds this budget it aborts rather than running away with cost.
    /// Equivalent to setting `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD`.
    ///
    /// Ignored when `--ai-backend` is not `claude-cli`.
    #[arg(long, global = true, value_name = "AMOUNT")]
    pub claude_cli_max_budget_usd: Option<f64>,

    /// Path to a single user-side `models.yaml` that short-circuits the
    /// standard `./.omni-dev/models.yaml` and `~/.omni-dev/models.yaml`
    /// lookup. The file is still merged over the embedded catalog.
    /// Equivalent to setting `OMNI_DEV_MODELS_YAML`.
    #[arg(long, global = true, value_name = "PATH")]
    pub models_yaml: Option<std::path::PathBuf>,

    /// Run as if omni-dev was started in `<PATH>` instead of the current
    /// working directory.
    ///
    /// Resolved exactly once here and threaded explicitly to each command as a
    /// parameter; deliberately **not** propagated to an environment variable
    /// (unlike the flags above) so the repo location never becomes an ambient
    /// global. Mirrors `git -C`.
    #[arg(long = "repo", short = 'C', global = true, value_name = "PATH")]
    pub repo: Option<std::path::PathBuf>,

    /// The main command to execute.
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level subcommand dispatch enum.
///
/// Each variant wraps the subcommand-specific argument struct (e.g.
/// [`ai::AiCommand`], [`git::GitCommand`], [`atlassian::AtlassianCommand`]);
/// follow the variant's payload type for the per-command argument surface.
#[derive(Subcommand)]
pub enum Commands {
    /// AI operations.
    Ai(ai::AiCommand),
    /// Git-related operations.
    Git(git::GitCommand),
    /// Command template management.
    Commands(commands::CommandsCommand),
    /// Configuration and model information.
    Config(config::ConfigCommand),
    /// Atlassian: JIRA and Confluence operations.
    Atlassian(atlassian::AtlassianCommand),
    /// Browser bridge: drive authenticated requests through a browser tab.
    Browser(browser::BrowserCommand),
    /// Daemon: host long-lived services (e.g. the browser bridge).
    Daemon(daemon::DaemonCommand),
    /// Datadog: read-only API operations.
    Datadog(datadog::DatadogCommand),
    /// Coverage: diff/patch coverage analysis for PR comments.
    Coverage(coverage::CoverageCommand),
    /// Transcript and caption fetching from media platforms.
    Transcript(transcript::TranscriptCommand),
    /// Embedded reference resources (specs, etc.).
    Resources(resources::ResourcesCommand),
    /// Generates shell completion scripts.
    #[command(hide = true)]
    Completions(completions::CompletionsCommand),
    /// Displays comprehensive help for all commands.
    #[command(name = "help-all")]
    HelpAll(help::HelpCommand),
}

impl Cli {
    /// Forwards global flags to the env vars that downstream factories
    /// read. Extracted so it can be unit-tested without invoking a real
    /// subcommand. Setting the env vars here (rather than threading extra
    /// arguments through every command) keeps factory signatures stable.
    fn propagate_global_flags(&self) {
        if let Some(backend) = self.ai_backend {
            match backend {
                AiBackend::Default => std::env::remove_var("OMNI_DEV_AI_BACKEND"),
                AiBackend::ClaudeCli => std::env::set_var("OMNI_DEV_AI_BACKEND", "claude-cli"),
            }
        }

        if self.claude_cli_allow_tools {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS", "true");
        }

        if self.claude_cli_allow_mcp {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_ALLOW_MCP", "true");
        }

        if let Some(budget) = self.claude_cli_max_budget_usd {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD", format!("{budget}"));
        }

        if let Some(path) = &self.models_yaml {
            std::env::set_var("OMNI_DEV_MODELS_YAML", path);
        }
    }

    /// Executes the CLI command.
    pub async fn execute(self) -> Result<()> {
        self.propagate_global_flags();

        // Resolve the repo location exactly once at this boundary, then thread
        // it explicitly into each command. Nothing deeper reads the ambient CWD.
        let Self { repo, command, .. } = self;
        let repo = repo.as_deref();

        match command {
            Commands::Ai(ai_cmd) => ai_cmd.execute().await,
            Commands::Git(git_cmd) => git_cmd.execute(repo).await,
            Commands::Commands(commands_cmd) => commands_cmd.execute(),
            Commands::Atlassian(cmd) => cmd.execute().await,
            Commands::Browser(cmd) => cmd.execute().await,
            Commands::Daemon(cmd) => cmd.execute().await,
            Commands::Datadog(cmd) => cmd.execute().await,
            Commands::Coverage(cmd) => cmd.execute(repo).await,
            Commands::Transcript(cmd) => cmd.execute().await,
            Commands::Config(config_cmd) => config_cmd.execute(),
            Commands::Resources(resources_cmd) => resources_cmd.execute(),
            Commands::Completions(completions_cmd) => completions_cmd.execute(),
            Commands::HelpAll(help_cmd) => help_cmd.execute(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_ai_backend_claude_cli() {
        let cli =
            Cli::try_parse_from(["omni-dev", "--ai-backend", "claude-cli", "help-all"]).unwrap();
        assert!(matches!(cli.ai_backend, Some(AiBackend::ClaudeCli)));
        assert!(!cli.claude_cli_allow_tools);
    }

    #[test]
    fn parses_ai_backend_default() {
        let cli = Cli::try_parse_from(["omni-dev", "--ai-backend", "default", "help-all"]).unwrap();
        assert!(matches!(cli.ai_backend, Some(AiBackend::Default)));
    }

    #[test]
    fn parses_ai_backend_absent() {
        let cli = Cli::try_parse_from(["omni-dev", "help-all"]).unwrap();
        assert!(cli.ai_backend.is_none());
        assert!(!cli.claude_cli_allow_tools);
        assert!(!cli.claude_cli_allow_mcp);
    }

    #[test]
    fn parses_claude_cli_allow_tools_flag() {
        let cli =
            Cli::try_parse_from(["omni-dev", "--claude-cli-allow-tools", "help-all"]).unwrap();
        assert!(cli.claude_cli_allow_tools);
    }

    #[test]
    fn parses_claude_cli_allow_mcp_flag() {
        let cli = Cli::try_parse_from(["omni-dev", "--claude-cli-allow-mcp", "help-all"]).unwrap();
        assert!(cli.claude_cli_allow_mcp);
        assert!(!cli.claude_cli_allow_tools);
    }

    #[test]
    fn allow_mcp_and_allow_tools_are_independent() {
        let only_mcp =
            Cli::try_parse_from(["omni-dev", "--claude-cli-allow-mcp", "help-all"]).unwrap();
        assert!(only_mcp.claude_cli_allow_mcp);
        assert!(!only_mcp.claude_cli_allow_tools);

        let only_tools =
            Cli::try_parse_from(["omni-dev", "--claude-cli-allow-tools", "help-all"]).unwrap();
        assert!(only_tools.claude_cli_allow_tools);
        assert!(!only_tools.claude_cli_allow_mcp);

        let both = Cli::try_parse_from([
            "omni-dev",
            "--claude-cli-allow-tools",
            "--claude-cli-allow-mcp",
            "help-all",
        ])
        .unwrap();
        assert!(both.claude_cli_allow_tools);
        assert!(both.claude_cli_allow_mcp);
    }

    #[test]
    fn global_flags_accepted_after_subcommand() {
        // clap global = true allows the flag before or after the subcommand.
        let cli = Cli::try_parse_from([
            "omni-dev",
            "help-all",
            "--ai-backend",
            "claude-cli",
            "--claude-cli-allow-tools",
        ])
        .unwrap();
        assert!(matches!(cli.ai_backend, Some(AiBackend::ClaudeCli)));
        assert!(cli.claude_cli_allow_tools);
    }

    #[test]
    fn parses_max_budget_usd_flag() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "--claude-cli-max-budget-usd",
            "0.50",
            "help-all",
        ])
        .unwrap();
        assert_eq!(cli.claude_cli_max_budget_usd, Some(0.50));
    }

    #[test]
    fn max_budget_usd_absent_is_none() {
        let cli = Cli::try_parse_from(["omni-dev", "help-all"]).unwrap();
        assert!(cli.claude_cli_max_budget_usd.is_none());
    }

    #[test]
    fn max_budget_usd_rejects_non_numeric() {
        let result = Cli::try_parse_from([
            "omni-dev",
            "--claude-cli-max-budget-usd",
            "cheap",
            "help-all",
        ]);
        let Err(err) = result else {
            panic!("expected parse error for non-numeric budget");
        };
        assert!(err.to_string().contains("invalid"));
    }

    // ── propagate_global_flags() tests ──
    //
    // These tests mutate process-global env vars, so they serialise on
    // `crate::claude::ai::claude_cli::CLI_ENV_LOCK` (shared with claude-cli's
    // own env-mutating tests to avoid cross-module races).

    const BACKEND_VAR: &str = "OMNI_DEV_AI_BACKEND";
    const ALLOW_TOOLS_VAR: &str = "OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS";
    const ALLOW_MCP_VAR: &str = "OMNI_DEV_CLAUDE_CLI_ALLOW_MCP";
    const MAX_BUDGET_VAR: &str = "OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD";
    const MODELS_YAML_VAR: &str = "OMNI_DEV_MODELS_YAML";

    /// Locks the shared mutex and snapshots/restores every env var
    /// `propagate_global_flags` may touch.
    struct GlobalFlagsEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: [(&'static str, Option<String>); 5],
    }

    impl GlobalFlagsEnvGuard {
        fn new() -> Self {
            let lock = crate::claude::ai::claude_cli::CLI_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let names = [
                BACKEND_VAR,
                ALLOW_TOOLS_VAR,
                ALLOW_MCP_VAR,
                MAX_BUDGET_VAR,
                MODELS_YAML_VAR,
            ];
            let saved = names.map(|n| (n, std::env::var(n).ok()));
            for (n, _) in &saved {
                std::env::remove_var(n);
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for GlobalFlagsEnvGuard {
        fn drop(&mut self) {
            for (n, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(n, v),
                    None => std::env::remove_var(n),
                }
            }
        }
    }

    fn cli_with_defaults() -> Cli {
        Cli::try_parse_from(["omni-dev", "help-all"]).unwrap()
    }

    #[test]
    fn propagate_global_flags_defaults_set_nothing() {
        let _g = GlobalFlagsEnvGuard::new();
        cli_with_defaults().propagate_global_flags();
        assert!(std::env::var(BACKEND_VAR).is_err());
        assert!(std::env::var(ALLOW_TOOLS_VAR).is_err());
        assert!(std::env::var(ALLOW_MCP_VAR).is_err());
        assert!(std::env::var(MAX_BUDGET_VAR).is_err());
        assert!(std::env::var(MODELS_YAML_VAR).is_err());
    }

    #[test]
    fn propagate_global_flags_sets_ai_backend_claude_cli() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.ai_backend = Some(AiBackend::ClaudeCli);
        cli.propagate_global_flags();
        assert_eq!(
            std::env::var(BACKEND_VAR).ok().as_deref(),
            Some("claude-cli")
        );
    }

    #[test]
    fn propagate_global_flags_default_backend_removes_env_var() {
        let _g = GlobalFlagsEnvGuard::new();
        std::env::set_var(BACKEND_VAR, "claude-cli");
        let mut cli = cli_with_defaults();
        cli.ai_backend = Some(AiBackend::Default);
        cli.propagate_global_flags();
        assert!(std::env::var(BACKEND_VAR).is_err());
    }

    #[test]
    fn propagate_global_flags_sets_allow_tools() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.claude_cli_allow_tools = true;
        cli.propagate_global_flags();
        assert_eq!(std::env::var(ALLOW_TOOLS_VAR).ok().as_deref(), Some("true"));
    }

    #[test]
    fn propagate_global_flags_sets_allow_mcp() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.claude_cli_allow_mcp = true;
        cli.propagate_global_flags();
        assert_eq!(std::env::var(ALLOW_MCP_VAR).ok().as_deref(), Some("true"));
    }

    #[test]
    fn propagate_global_flags_sets_max_budget_usd() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.claude_cli_max_budget_usd = Some(1.5);
        cli.propagate_global_flags();
        assert_eq!(std::env::var(MAX_BUDGET_VAR).ok().as_deref(), Some("1.5"));
    }

    #[test]
    fn parses_models_yaml_flag() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "--models-yaml",
            "/tmp/custom-models.yaml",
            "help-all",
        ])
        .unwrap();
        assert_eq!(
            cli.models_yaml.as_deref(),
            Some(std::path::Path::new("/tmp/custom-models.yaml"))
        );
    }

    #[test]
    fn parses_repo_flag_long_and_short() {
        let long = Cli::try_parse_from(["omni-dev", "--repo", "/tmp/r", "help-all"]).unwrap();
        assert_eq!(
            long.repo.as_deref(),
            Some(std::path::Path::new("/tmp/r")),
            "--repo should populate cli.repo"
        );
        let short = Cli::try_parse_from(["omni-dev", "-C", "/tmp/r", "help-all"]).unwrap();
        assert_eq!(
            short.repo.as_deref(),
            Some(std::path::Path::new("/tmp/r")),
            "-C should populate cli.repo"
        );
        let absent = Cli::try_parse_from(["omni-dev", "help-all"]).unwrap();
        assert!(absent.repo.is_none());
    }

    /// RULE 3: the repo location is a parameter, never a relocated global.
    /// `propagate_global_flags` must not export it to any environment variable.
    #[test]
    fn repo_flag_is_not_propagated_to_env() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.repo = Some(std::path::PathBuf::from("/tmp/some-repo"));
        cli.propagate_global_flags();
        assert!(
            std::env::var("OMNI_DEV_REPO").is_err(),
            "repo must not be exported to an env var"
        );
    }

    #[test]
    fn propagate_global_flags_sets_models_yaml() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.models_yaml = Some(std::path::PathBuf::from("/tmp/custom-models.yaml"));
        cli.propagate_global_flags();
        assert_eq!(
            std::env::var(MODELS_YAML_VAR).ok().as_deref(),
            Some("/tmp/custom-models.yaml")
        );
    }

    #[test]
    fn propagate_global_flags_independent_flags_compose() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.ai_backend = Some(AiBackend::ClaudeCli);
        cli.claude_cli_allow_tools = true;
        cli.claude_cli_allow_mcp = true;
        cli.claude_cli_max_budget_usd = Some(0.25);
        cli.propagate_global_flags();
        assert_eq!(
            std::env::var(BACKEND_VAR).ok().as_deref(),
            Some("claude-cli")
        );
        assert_eq!(std::env::var(ALLOW_TOOLS_VAR).ok().as_deref(), Some("true"));
        assert_eq!(std::env::var(ALLOW_MCP_VAR).ok().as_deref(), Some("true"));
        assert_eq!(std::env::var(MAX_BUDGET_VAR).ok().as_deref(), Some("0.25"));
    }
}
