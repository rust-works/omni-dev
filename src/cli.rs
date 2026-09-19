//! CLI interface for omni-dev.

use anyhow::Result;
use clap::{Parser, Subcommand};

pub mod ai;
pub mod ai_backend_args;
pub mod atlassian;
pub mod browser;
pub mod commands;
pub mod completions;
pub mod config;
pub(crate) mod confirm;
pub mod coverage;
// The daemon and the Snowflake client (which talks to the daemon over its
// Unix-domain control socket) are Unix-only; on Windows they run only under WSL2,
// and a native (non-WSL) Windows port is future work (#1363).
#[cfg(unix)]
pub mod claude_wrap;
#[cfg(unix)]
pub mod daemon;
pub mod datadog;
pub mod drive;
pub mod format;
pub mod git;
pub mod gmail;
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
/// Global flags (`--profile`, `--instance`) are propagated to environment
/// variables read by downstream factories before dispatching to a
/// [`Commands`] variant. The AI backend flags are per-command, via
/// [`ai_backend_args::AiBackendArgs`] (#1778).
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
    /// Drive: search and read Google Drive files via OAuth2 (read-only).
    Drive(drive::DriveCommand),
    /// Gmail: search, read, and label messages via OAuth2.
    Gmail(gmail::GmailCommand),
    /// Snowflake: run arbitrary SQL through the daemon's multiplexed sessions.
    #[cfg(unix)]
    Snowflake(snowflake::SnowflakeCommand),
    /// Worktrees: list the repos/worktrees open across all VS Code windows.
    #[cfg(unix)]
    Worktrees(worktrees::WorktreesCommand),
    /// Sessions: track Claude Code sessions running across all terminals and windows.
    #[cfg(unix)]
    Sessions(sessions::SessionsCommand),
    /// Wrap the Claude process, reporting its exact session state to the daemon.
    #[cfg(unix)]
    #[command(name = "claude-wrap")]
    ClaudeWrap(claude_wrap::ClaudeWrapCommand),
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
    /// `~/.omni-dev/settings.json`, degrading to defaults when it is absent
    /// (silently) or unreadable/unparseable (with a warning) rather than
    /// failing (an unreadable settings file must not block commands that use
    /// no profile). A named function so it can be unit-tested directly instead
    /// of as an inline closure.
    fn load_settings_or_default() -> crate::utils::settings::Settings {
        crate::utils::settings::Settings::load_or_warn_default()
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
            Commands::Drive(cmd) => cmd.execute().await,
            Commands::Gmail(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::Snowflake(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::Worktrees(cmd) => cmd.execute(repo).await,
            #[cfg(unix)]
            Commands::Sessions(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::ClaudeWrap(cmd) => cmd.execute().await,
            Commands::Coverage(cmd) => cmd.execute(repo),
            Commands::Transcript(cmd) => cmd.execute().await,
            Commands::Log(log_cmd) => log_cmd.execute(),
            Commands::Config(config_cmd) => config_cmd.execute(repo),
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

    // `execute()`'s command dispatch is otherwise only exercised by spawning
    // the real binary in integration tests; this covers the `Gmail` arm
    // in-process (deterministic, network-free: missing credentials fail
    // fast before any Gmail API call).
    #[tokio::test]
    async fn execute_routes_gmail_subcommand() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cli = Cli::try_parse_from(["omni-dev", "gmail", "auth", "status"]).unwrap();
        let err = cli.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    // `execute()`'s command dispatch is otherwise only exercised by spawning
    // the real binary in integration tests; this covers the `Drive` arm
    // in-process (deterministic, network-free: missing credentials fail
    // fast before any Drive API call).
    #[tokio::test]
    async fn execute_routes_drive_subcommand() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cli = Cli::try_parse_from(["omni-dev", "drive", "auth", "status"]).unwrap();
        let err = cli.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    // ── global arg-id collision tests (#1420) ──

    /// A `global = true` arg is propagated by clap **arg id**, and the derive's
    /// id defaults to the field name — so a subcommand-local field named `repo`
    /// displaced the global `-C/--repo` under `worktrees register` and its
    /// `String` was copied back up into the root matches, panicking `Cli`'s
    /// `PathBuf` read. Renaming the local field to `repo_name` (`--repo-name`)
    /// separates the ids; both spellings must now parse side by side.
    #[cfg(unix)]
    #[test]
    fn worktrees_register_repo_name_coexists_with_global_repo() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "-C",
            "/tmp/somerepo",
            "worktrees",
            "register",
            "--key",
            "k1",
            "--repo-name",
            "myrepo",
            "--folder",
            "/tmp",
        ])
        .unwrap();
        assert_eq!(
            cli.repo.as_deref(),
            Some(std::path::Path::new("/tmp/somerepo"))
        );
        let Commands::Worktrees(worktrees::WorktreesCommand {
            command: worktrees::WorktreesSubcommands::Register(register),
        }) = cli.command
        else {
            panic!("expected a `worktrees register` invocation");
        };
        assert_eq!(register.key, "k1");
        assert_eq!(register.repo_name.as_deref(), Some("myrepo"));

        // The issue's exact repro, which panicked outright: the local flag with
        // no global alongside it leaves the global unset rather than shadowed.
        let cli = Cli::try_parse_from([
            "omni-dev",
            "worktrees",
            "register",
            "--key",
            "k1",
            "--repo-name",
            "myrepo",
            "--folder",
            "/tmp",
        ])
        .unwrap();
        assert!(cli.repo.is_none());
    }

    /// Generalises the #1420 audit: no subcommand anywhere in the tree may
    /// define an arg whose id collides with a root `global = true` arg. The
    /// failure mode is silent at parse time and only surfaces as a downcast
    /// panic when the global is read, so it is worth pinning structurally.
    ///
    /// Walks the **un-built** `Command`: `Command::build` propagates the globals
    /// into every subcommand, which would make the check vacuously pass.
    #[test]
    fn no_subcommand_arg_shadows_a_global_arg_id() {
        use clap::CommandFactory;
        use std::collections::HashSet;

        // A global arg isn't only declared at the root (`--profile`,
        // `--instance`, …) — a subcommand can scope its own `global = true`
        // arg to just its subtree (e.g. `gmail`'s `--account`, inherited by
        // every `gmail` subcommand but not by sibling top-level commands
        // like `snowflake`). So globals accumulate as the walk descends,
        // not just once at the root.
        //
        // Two distinct clap failure modes share this same root cause and
        // are both checked here:
        // - id collision: a subcommand-local arg id equal to an inherited
        //   global's id silently shadows it at read time instead of
        //   erroring (#1420).
        // - long-flag collision: two args bound to the same `--flag`
        //   string on one effective command is a hard clap panic
        //   ("Long option names must be unique for each argument") — this
        //   is the *actual* constraint; a differently-named id with the
        //   same `long` still collides.
        fn walk(
            cmd: &clap::Command,
            inherited_ids: &HashSet<String>,
            inherited_longs: &HashSet<String>,
            path: &str,
        ) {
            let mut ids = inherited_ids.clone();
            let mut longs = inherited_longs.clone();
            for arg in cmd.get_arguments().filter(|a| a.is_global_set()) {
                ids.insert(arg.get_id().as_str().to_string());
                if let Some(long) = arg.get_long() {
                    longs.insert(long.to_string());
                }
            }

            for sub in cmd.get_subcommands() {
                let sub_path = format!("{path} {}", sub.get_name());
                for arg in sub.get_arguments() {
                    let id = arg.get_id().as_str();
                    assert!(
                        !ids.contains(id),
                        "`{sub_path}` defines an arg with id `{id}`, which is an \
                         inherited global arg id — clap propagates globals by id, \
                         so this shadows the global and panics when it is read \
                         (#1420). Rename the subcommand-local field.",
                    );
                    if let Some(long) = arg.get_long() {
                        assert!(
                            !longs.contains(long),
                            "`{sub_path}` defines `--{long}`, already an inherited \
                             global flag — clap rejects two args on the same \
                             command sharing a long flag name (issue #1500). \
                             Rename the subcommand-local flag.",
                        );
                    }
                }
                walk(sub, &ids, &longs, &sub_path);
            }
        }

        let cmd = Cli::command();
        let root_globals: HashSet<String> = cmd
            .get_arguments()
            .filter(|a| a.is_global_set())
            .map(|a| a.get_id().as_str().to_string())
            .collect();
        assert!(
            !root_globals.is_empty(),
            "expected the root command to declare global args"
        );
        walk(&cmd, &HashSet::new(), &HashSet::new(), "omni-dev");
    }

    // ── propagate_global_flags() tests ──
    //
    // These tests mutate process-global env vars, so they serialise on
    // `crate::claude::ai::claude_cli::CLI_ENV_LOCK` (shared with claude-cli's
    // own env-mutating tests to avoid cross-module races).

    const PROFILE_VAR: &str = "OMNI_DEV_PROFILE";
    const INSTANCE_VAR: &str = "OMNI_DEV_ATLASSIAN_INSTANCE";

    /// Locks the shared mutex and snapshots/restores every env var
    /// `propagate_global_flags` may touch.
    struct GlobalFlagsEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: [(&'static str, Option<String>); 2],
    }

    impl GlobalFlagsEnvGuard {
        fn new() -> Self {
            let lock = crate::claude::ai::claude_cli::CLI_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let names = [PROFILE_VAR, INSTANCE_VAR];
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
}
