//! CLI interface for omni-dev.

use anyhow::Result;
use clap::{Parser, Subcommand};

pub mod ai;
pub mod atlassian;
pub mod browser;
pub mod commands;
pub mod completions;
pub mod config;
pub mod coverage;
// The daemon and the Snowflake client (which talks to the daemon over its
// Unix-domain control socket) are Unix-only; on Windows they run only under WSL2,
// and a native (non-WSL) Windows port is future work (#1363).
#[cfg(unix)]
pub mod daemon;
pub mod datadog;
pub mod format;
pub mod git;
pub mod help;
pub mod log;
pub mod resources;
#[cfg(unix)]
pub mod sessions;
#[cfg(unix)]
pub mod snowflake;
pub mod transcript;
#[cfg(unix)]
pub mod worktrees;

// The `--ai-backend` value enum lives with the shared backend/model resolver;
// re-exported here so `crate::cli::AiBackend` keeps working.
pub use crate::claude::backend::AiBackend;

/// Top-level clap-derived CLI struct; the library entry point for embedding
/// omni-dev programmatically.
///
/// Global flags (`--ai-backend`, `--model`, `--beta-header`,
/// `--claude-cli-allow-tools`, `--claude-cli-allow-mcp`,
/// `--claude-cli-max-budget-usd`, `--models-yaml`) are propagated to
/// environment variables read by downstream factories before dispatching to a
/// [`Commands`] variant.
#[derive(Parser)]
#[command(name = "omni-dev")]
#[command(
    about = "AI-powered git commit rewriter, PR generator, and MCP server for Jira, Confluence, and Datadog.",
    long_about = None
)]
// `-V` shows the bare crate version; `--version` adds git provenance (commit,
// date, dirty flag) so a local/unreleased build is identifiable (#1374).
#[command(version = crate::VERSION, long_version = crate::build_info::long_version())]
pub struct Cli {
    /// Selects the AI backend used by commands that invoke an AI model.
    ///
    /// Overrides the `OMNI_DEV_AI_BACKEND` environment variable and the
    /// legacy `USE_OPENAI`/`USE_OLLAMA`/`CLAUDE_CODE_USE_BEDROCK` variables
    /// (`default` forces the direct Anthropic API even when they are set).
    #[arg(long, global = true, value_enum)]
    pub ai_backend: Option<AiBackend>,

    /// AI model to use for commands that invoke an AI model.
    ///
    /// Highest-precedence model selector: it overrides `OMNI_DEV_MODEL` and
    /// every per-backend model variable (`CLAUDE_MODEL`, `CLAUDE_CODE_MODEL`,
    /// `ANTHROPIC_MODEL`, `OPENAI_MODEL`, `OLLAMA_MODEL`). Equivalent to
    /// setting `OMNI_DEV_MODEL`.
    #[arg(long, global = true, value_name = "MODEL")]
    pub model: Option<String>,

    /// Beta header to send with AI API requests (format: key:value).
    ///
    /// Only sent if the model supports it in the model registry. Equivalent
    /// to setting `OMNI_DEV_BETA_HEADER`. Ignored when `--ai-backend` is
    /// `claude-cli` (the CLI negotiates betas itself).
    #[arg(long, global = true, value_name = "KEY:VALUE")]
    pub beta_header: Option<String>,

    /// Weakens the `claude-cli` sandbox by allowing the nested `claude -p`
    /// session to use its default built-in tools (Read, Edit, Write, Bash,
    /// Glob, Grep).
    ///
    /// **Only use for deliberately tool-capable use cases.** By default the
    /// nested session runs with `--tools ""` and cannot touch the
    /// file system. This flag removes that guard. The prompt is built from
    /// untrusted content (diffs, commit messages, JIRA text), so well-known
    /// secret env vars (`*_API_KEY`, `*_TOKEN`, etc.) are scrubbed from the
    /// nested session; set `OMNI_DEV_CLAUDE_CLI_KEEP_ENV` to exempt names.
    /// Equivalent to setting `OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS=true`.
    /// Independent of `--claude-cli-allow-mcp`.
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

    /// Selects a named credential/config profile from
    /// `~/.omni-dev/settings.json` (AWS-CLI style).
    ///
    /// When set, the profile's `env` bundle replaces the base `env` map in the
    /// settings-fallback chain (process env still wins); the base map is not
    /// consulted. Overrides `OMNI_DEV_PROFILE`. An unknown name is a hard error
    /// listing the known profiles.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Overrides the Atlassian instance URL (e.g.
    /// `https://org.atlassian.net`) for every JIRA and Confluence command.
    ///
    /// Takes precedence over `ATLASSIAN_INSTANCE_URL` / settings.json (email
    /// and API token still come from the environment/settings). Lets a
    /// multi-site user target a specific tenant per invocation. Equivalent to
    /// setting `OMNI_DEV_ATLASSIAN_INSTANCE`. Ignored by non-Atlassian
    /// commands.
    #[arg(long, global = true, value_name = "URL")]
    pub instance: Option<String>,

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
    #[cfg(unix)]
    Daemon(daemon::DaemonCommand),
    /// Datadog: read-only API operations.
    Datadog(datadog::DatadogCommand),
    /// Snowflake: run arbitrary SQL through the daemon's multiplexed sessions.
    #[cfg(unix)]
    Snowflake(snowflake::SnowflakeCommand),
    /// Worktrees: list the repos/worktrees open across all VS Code windows.
    #[cfg(unix)]
    Worktrees(worktrees::WorktreesCommand),
    /// Sessions: track Claude Code sessions running across all terminals and windows.
    #[cfg(unix)]
    Sessions(sessions::SessionsCommand),
    /// Coverage: diff/patch coverage analysis for PR comments.
    Coverage(coverage::CoverageCommand),
    /// Transcript and caption fetching from media platforms.
    Transcript(transcript::TranscriptCommand),
    /// Search the local invocation + HTTP request log.
    Log(log::LogCommand),
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
        // Every value — including `default` — is written to the env var so
        // the flag decisively overrides both a pre-set OMNI_DEV_AI_BACKEND
        // and the legacy USE_* selection flags (#1118).
        if let Some(backend) = self.ai_backend {
            std::env::set_var(crate::claude::backend::AI_BACKEND_ENV, backend.env_value());
        }

        if let Some(model) = &self.model {
            std::env::set_var(crate::claude::backend::MODEL_ENV, model);
        }

        if let Some(beta_header) = &self.beta_header {
            std::env::set_var(crate::claude::backend::BETA_HEADER_ENV, beta_header);
        }

        // The escape-hatch exports are also recorded in the flag-provenance
        // registry so the sandbox-weakened WARN can attribute them to the
        // flag rather than to an ambient shell export (issue #1143).
        if self.claude_cli_allow_tools {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS", "true");
            crate::utils::settings::note_cli_flag_export("OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS");
        }

        if self.claude_cli_allow_mcp {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_ALLOW_MCP", "true");
            crate::utils::settings::note_cli_flag_export("OMNI_DEV_CLAUDE_CLI_ALLOW_MCP");
        }

        if let Some(budget) = self.claude_cli_max_budget_usd {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD", format!("{budget}"));
        }

        if let Some(path) = &self.models_yaml {
            std::env::set_var("OMNI_DEV_MODELS_YAML", path);
        }

        // The flag beats the env var: setting OMNI_DEV_PROFILE here means the
        // settings readers (which discover the active profile from that env
        // var) pick up the flag. When the flag is absent we leave any existing
        // OMNI_DEV_PROFILE untouched, so the env-var path still works.
        if let Some(profile) = &self.profile {
            std::env::set_var(crate::utils::settings::PROFILE_ENV_VAR, profile);
        }

        // The global `--instance` flag overrides the configured Atlassian
        // instance for every JIRA/Confluence command. Propagated to the env var
        // that `atlassian::auth::load_credentials` reads (#1117). When absent we
        // leave any existing value untouched so the env-var path still works.
        if let Some(instance) = &self.instance {
            std::env::set_var(
                crate::atlassian::auth::ATLASSIAN_INSTANCE_OVERRIDE_ENV,
                instance,
            );
        }
    }

    /// Validates the active profile (resolved from `env`) against the settings
    /// produced by `load_settings`. The loader is invoked only when a profile is
    /// actually active, so a no-profile invocation reads no disk. Pure over its
    /// inputs — unit-tested with a `MapEnv` and a constructed `Settings` rather
    /// than the process environment and `~/.omni-dev/settings.json`.
    fn validate_active_profile<E, F>(env: &E, load_settings: F) -> Result<()>
    where
        E: crate::utils::env::EnvSource,
        F: FnOnce() -> crate::utils::settings::Settings,
    {
        match crate::utils::settings::active_profile_from(env) {
            Some(name) => load_settings().validate_profile(&name),
            None => Ok(()),
        }
    }

    /// Thin disk boundary for [`Self::validate_active_profile`]: loads
    /// `~/.omni-dev/settings.json`, degrading to defaults when it is absent or
    /// unreadable rather than failing (an unreadable settings file must not block
    /// commands that use no profile). A named function so it can be unit-tested
    /// directly instead of as an inline closure.
    fn load_settings_or_default() -> crate::utils::settings::Settings {
        crate::utils::settings::Settings::load().unwrap_or_default()
    }

    /// Executes the CLI command.
    pub async fn execute(self) -> Result<()> {
        self.propagate_global_flags();

        // Validate the selected profile once, before dispatch, so a typo fails
        // fast rather than silently falling back to base credentials. The loader
        // runs only when a profile is active, so a no-profile invocation pays no
        // extra disk I/O.
        Self::validate_active_profile(
            &crate::utils::env::SystemEnv,
            Self::load_settings_or_default,
        )?;

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
            #[cfg(unix)]
            Commands::Daemon(cmd) => cmd.execute().await,
            Commands::Datadog(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::Snowflake(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::Worktrees(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::Sessions(cmd) => cmd.execute().await,
            Commands::Coverage(cmd) => cmd.execute(repo).await,
            Commands::Transcript(cmd) => cmd.execute().await,
            Commands::Log(log_cmd) => log_cmd.execute(),
            Commands::Config(config_cmd) => config_cmd.execute(),
            Commands::Resources(resources_cmd) => resources_cmd.execute(),
            Commands::Completions(completions_cmd) => completions_cmd.execute(),
            Commands::HelpAll(help_cmd) => help_cmd.execute(),
        }
    }
}

#[cfg(all(target_os = "macos", feature = "menu-bar"))]
impl Cli {
    /// If this invocation is `daemon run` without `--no-menu`, resolves the
    /// daemon configuration so `main` can host it with a macOS menu-bar tray on
    /// the main thread. Returns `None` for every other invocation (which runs
    /// normally on the async runtime).
    pub fn menu_bar_run_config(&self) -> Option<Result<crate::daemon::DaemonRunConfig>> {
        match &self.command {
            Commands::Daemon(daemon::DaemonCommand {
                command: daemon::DaemonSubcommands::Run(run),
            }) if !run.no_menu => Some(run.clone().into_run_config()),
            _ => None,
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
    fn parses_ai_backend_openai_ollama_bedrock() {
        for (value, expected) in [
            ("openai", AiBackend::OpenAi),
            ("ollama", AiBackend::Ollama),
            ("bedrock", AiBackend::Bedrock),
        ] {
            let cli = Cli::try_parse_from(["omni-dev", "--ai-backend", value, "help-all"]).unwrap();
            assert_eq!(cli.ai_backend, Some(expected), "value {value}");
        }
    }

    #[test]
    fn parses_model_before_and_after_subcommand() {
        // Before the subcommand — the placement the docs show
        // (`omni-dev --model … git commit message twiddle …`), broken
        // pre-#1118 because --model was subcommand-local.
        let before = Cli::try_parse_from([
            "omni-dev",
            "--model",
            "claude-opus-4-6",
            "git",
            "commit",
            "message",
            "twiddle",
        ])
        .unwrap();
        assert_eq!(before.model.as_deref(), Some("claude-opus-4-6"));

        // After the subcommand — the pre-#1118 placement keeps parsing
        // because the arg is global = true.
        let after = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "twiddle",
            "--model",
            "claude-opus-4-6",
        ])
        .unwrap();
        assert_eq!(after.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn parses_beta_header_before_and_after_subcommand() {
        let before = Cli::try_parse_from([
            "omni-dev",
            "--beta-header",
            "anthropic-beta:output-128k-2025-02-19",
            "git",
            "commit",
            "message",
            "check",
        ])
        .unwrap();
        assert_eq!(
            before.beta_header.as_deref(),
            Some("anthropic-beta:output-128k-2025-02-19")
        );

        let after = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "check",
            "--beta-header",
            "anthropic-beta:output-128k-2025-02-19",
        ])
        .unwrap();
        assert_eq!(
            after.beta_header.as_deref(),
            Some("anthropic-beta:output-128k-2025-02-19")
        );
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
    const MODEL_VAR: &str = "OMNI_DEV_MODEL";
    const BETA_HEADER_VAR: &str = "OMNI_DEV_BETA_HEADER";
    const ALLOW_TOOLS_VAR: &str = "OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS";
    const ALLOW_MCP_VAR: &str = "OMNI_DEV_CLAUDE_CLI_ALLOW_MCP";
    const MAX_BUDGET_VAR: &str = "OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD";
    const MODELS_YAML_VAR: &str = "OMNI_DEV_MODELS_YAML";
    const PROFILE_VAR: &str = "OMNI_DEV_PROFILE";
    const INSTANCE_VAR: &str = "OMNI_DEV_ATLASSIAN_INSTANCE";

    /// Locks the shared mutex and snapshots/restores every env var
    /// `propagate_global_flags` may touch.
    struct GlobalFlagsEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: [(&'static str, Option<String>); 9],
    }

    impl GlobalFlagsEnvGuard {
        fn new() -> Self {
            let lock = crate::claude::ai::claude_cli::CLI_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let names = [
                BACKEND_VAR,
                MODEL_VAR,
                BETA_HEADER_VAR,
                ALLOW_TOOLS_VAR,
                ALLOW_MCP_VAR,
                MAX_BUDGET_VAR,
                MODELS_YAML_VAR,
                PROFILE_VAR,
                INSTANCE_VAR,
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
        assert!(std::env::var(MODEL_VAR).is_err());
        assert!(std::env::var(BETA_HEADER_VAR).is_err());
        assert!(std::env::var(ALLOW_TOOLS_VAR).is_err());
        assert!(std::env::var(ALLOW_MCP_VAR).is_err());
        assert!(std::env::var(MAX_BUDGET_VAR).is_err());
        assert!(std::env::var(MODELS_YAML_VAR).is_err());
        assert!(std::env::var(PROFILE_VAR).is_err());
        assert!(std::env::var(INSTANCE_VAR).is_err());
    }

    #[test]
    fn propagate_global_flags_sets_instance() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.instance = Some("https://org.atlassian.net".to_string());
        cli.propagate_global_flags();
        assert_eq!(
            std::env::var(INSTANCE_VAR).ok().as_deref(),
            Some("https://org.atlassian.net")
        );
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
    fn propagate_global_flags_default_backend_overrides_env_var() {
        // `--ai-backend default` must *set* the env var (not remove it) so it
        // decisively overrides both a pre-set backend and the legacy USE_*
        // flags (#1118).
        let _g = GlobalFlagsEnvGuard::new();
        std::env::set_var(BACKEND_VAR, "claude-cli");
        let mut cli = cli_with_defaults();
        cli.ai_backend = Some(AiBackend::Default);
        cli.propagate_global_flags();
        assert_eq!(std::env::var(BACKEND_VAR).ok().as_deref(), Some("default"));
    }

    #[test]
    fn propagate_global_flags_sets_openai_ollama_bedrock() {
        let _g = GlobalFlagsEnvGuard::new();
        for (backend, expected) in [
            (AiBackend::OpenAi, "openai"),
            (AiBackend::Ollama, "ollama"),
            (AiBackend::Bedrock, "bedrock"),
        ] {
            let mut cli = cli_with_defaults();
            cli.ai_backend = Some(backend);
            cli.propagate_global_flags();
            assert_eq!(std::env::var(BACKEND_VAR).ok().as_deref(), Some(expected));
        }
    }

    #[test]
    fn propagate_global_flags_sets_model() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.model = Some("claude-opus-4-6".to_string());
        cli.propagate_global_flags();
        assert_eq!(
            std::env::var(MODEL_VAR).ok().as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[test]
    fn propagate_global_flags_sets_beta_header() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.beta_header = Some("anthropic-beta:output-128k-2025-02-19".to_string());
        cli.propagate_global_flags();
        assert_eq!(
            std::env::var(BETA_HEADER_VAR).ok().as_deref(),
            Some("anthropic-beta:output-128k-2025-02-19")
        );
    }

    #[test]
    fn propagate_global_flags_sets_allow_tools() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.claude_cli_allow_tools = true;
        cli.propagate_global_flags();
        assert_eq!(std::env::var(ALLOW_TOOLS_VAR).ok().as_deref(), Some("true"));
        // The flag export is recorded for WARN provenance (issue #1143). The
        // registry is additive-only, so this assertion is order-independent.
        assert!(crate::utils::settings::exported_by_cli_flag(
            ALLOW_TOOLS_VAR
        ));
    }

    #[test]
    fn propagate_global_flags_sets_allow_mcp() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.claude_cli_allow_mcp = true;
        cli.propagate_global_flags();
        assert_eq!(std::env::var(ALLOW_MCP_VAR).ok().as_deref(), Some("true"));
        assert!(crate::utils::settings::exported_by_cli_flag(ALLOW_MCP_VAR));
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
    fn parses_profile_flag() {
        let cli = Cli::try_parse_from(["omni-dev", "--profile", "work", "help-all"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
    }

    #[test]
    fn profile_absent_is_none() {
        let cli = Cli::try_parse_from(["omni-dev", "help-all"]).unwrap();
        assert!(cli.profile.is_none());
    }

    #[test]
    fn propagate_global_flags_sets_profile() {
        let _g = GlobalFlagsEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.profile = Some("work".to_string());
        cli.propagate_global_flags();
        assert_eq!(std::env::var(PROFILE_VAR).ok().as_deref(), Some("work"));
    }

    #[test]
    fn propagate_global_flags_profile_flag_beats_env_var() {
        let _g = GlobalFlagsEnvGuard::new();
        std::env::set_var(PROFILE_VAR, "personal");
        let mut cli = cli_with_defaults();
        cli.profile = Some("work".to_string());
        cli.propagate_global_flags();
        assert_eq!(std::env::var(PROFILE_VAR).ok().as_deref(), Some("work"));
    }

    #[test]
    fn propagate_global_flags_absent_profile_leaves_env_var() {
        let _g = GlobalFlagsEnvGuard::new();
        std::env::set_var(PROFILE_VAR, "personal");
        cli_with_defaults().propagate_global_flags();
        assert_eq!(std::env::var(PROFILE_VAR).ok().as_deref(), Some("personal"));
    }

    // ── validate_active_profile() seam (pure: MapEnv + injected settings loader,
    // no process env, no disk) ──

    #[test]
    fn validate_active_profile_ok_and_skips_load_when_no_profile() {
        use crate::test_support::env::MapEnv;
        let env = MapEnv::new();
        let result = Cli::validate_active_profile(&env, || panic!("must not load settings"));
        assert!(result.is_ok());
    }

    #[test]
    fn validate_active_profile_ok_for_known_profile() {
        use crate::test_support::env::MapEnv;
        use crate::utils::settings::{Profile, Settings};
        let env = MapEnv::new().with(PROFILE_VAR, "work");
        let settings = Settings {
            profiles: std::iter::once(("work".to_string(), Profile::default())).collect(),
            ..Default::default()
        };
        assert!(Cli::validate_active_profile(&env, || settings).is_ok());
    }

    #[test]
    fn validate_active_profile_errors_for_unknown_profile() {
        use crate::test_support::env::MapEnv;
        use crate::utils::settings::Settings;
        let env = MapEnv::new().with(PROFILE_VAR, "wrok");
        let err = Cli::validate_active_profile(&env, Settings::default)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown profile 'wrok'"));
    }

    #[test]
    fn load_settings_or_default_never_panics() {
        // The disk boundary must degrade to defaults rather than panic when
        // `~/.omni-dev/settings.json` is absent or unreadable. Exercises the
        // production loader directly, no process env or fixture required.
        let _settings = Cli::load_settings_or_default();
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
