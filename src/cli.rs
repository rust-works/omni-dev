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
pub mod repo_arg;
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
/// The only global flag is `--profile`, the one flag every credentialed
/// command reads; it is propagated to `OMNI_DEV_PROFILE` before dispatching to
/// a [`Commands`] variant. Every other flag is declared only on the commands
/// that read it — see the placement rule in [`ai_backend_args`] (#1778).
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
    /// Forwards `--profile` to the env var the settings readers discover the
    /// active profile from. Extracted so it can be unit-tested without
    /// invoking a real subcommand.
    fn propagate_profile_flag(&self) {
        // The flag beats the env var: setting OMNI_DEV_PROFILE here means the
        // settings readers (which discover the active profile from that env
        // var) pick up the flag. When the flag is absent we leave any existing
        // OMNI_DEV_PROFILE untouched, so the env-var path still works.
        if let Some(profile) = &self.profile {
            std::env::set_var(crate::utils::settings::PROFILE_ENV_VAR, profile);
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
        self.propagate_profile_flag();

        // Validate the selected profile once, before dispatch, so a typo fails
        // fast rather than silently falling back to base credentials. The loader
        // runs only when a profile is active, so a no-profile invocation pays no
        // extra disk I/O.
        Self::validate_active_profile(
            &crate::utils::env::SystemEnv,
            Self::load_settings_or_default,
        )?;

        match self.command {
            Commands::Ai(ai_cmd) => ai_cmd.execute().await,
            Commands::Git(git_cmd) => git_cmd.execute().await,
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
            Commands::Worktrees(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::Sessions(cmd) => cmd.execute().await,
            #[cfg(unix)]
            Commands::ClaudeWrap(cmd) => cmd.execute().await,
            Commands::Coverage(cmd) => cmd.execute(),
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
    /// displaced the then-root-global `-C/--repo` under `worktrees register`
    /// and panicked (#1420); the local field became `repo_name`
    /// (`--repo-name`). Since #1778 `-C/--repo` is scoped to the commands that
    /// read it, which `worktrees register` is not, so it now rejects `-C`
    /// outright while `--repo-name` keeps parsing.
    #[cfg(unix)]
    #[test]
    fn worktrees_register_takes_repo_name_but_not_repo() {
        let register = [
            "worktrees",
            "register",
            "--key",
            "k1",
            "--repo-name",
            "myrepo",
            "--folder",
            "/tmp",
        ];
        let cli = Cli::try_parse_from(std::iter::once("omni-dev").chain(register)).unwrap();
        let Commands::Worktrees(worktrees::WorktreesCommand {
            command: worktrees::WorktreesSubcommands::Register(register_cmd),
        }) = cli.command
        else {
            panic!("expected a `worktrees register` invocation");
        };
        assert_eq!(register_cmd.key, "k1");
        assert_eq!(register_cmd.repo_name.as_deref(), Some("myrepo"));

        let with_repo = std::iter::once("omni-dev")
            .chain(register)
            .chain(["-C", "/tmp/somerepo"]);
        assert!(Cli::try_parse_from(with_repo).is_err());
    }

    /// Pins which leaf commands accept each scoped flag (#1778). Every flag
    /// except `--profile` is declared only on the commands that read it, so a
    /// new command can only gain one on purpose — and a flag that silently
    /// spreads to a command that ignores it fails here.
    #[test]
    fn scoped_flags_are_accepted_only_by_their_readers() {
        use clap::CommandFactory;

        fn leaves(cmd: &clap::Command, path: &str, out: &mut Vec<(String, Vec<String>)>) {
            let subs: Vec<_> = cmd
                .get_subcommands()
                .filter(|s| s.get_name() != "help")
                .collect();
            if subs.is_empty() {
                let longs = cmd
                    .get_arguments()
                    .filter_map(|a| a.get_long().map(str::to_string))
                    .collect();
                out.push((path.to_string(), longs));
            }
            for sub in subs {
                leaves(sub, &format!("{path} {}", sub.get_name()), out);
            }
        }

        // `build` propagates every (subtree-)global arg into its descendants,
        // so each leaf's argument list is exactly what it accepts.
        let mut cmd = Cli::command();
        cmd.build();
        let mut all = Vec::new();
        leaves(&cmd, "omni-dev", &mut all);

        let under = |path: &str, owners: &[&str]| {
            owners.iter().any(|o| {
                let o = format!("omni-dev {o}");
                path == o || path.starts_with(&format!("{o} "))
            })
        };
        const AI: &[&str] = &[
            "git commit message twiddle",
            "git commit message check",
            "git commit message staged",
            "git branch create pr",
            "ai chat",
            "ai jev verify-decision",
        ];
        let expected: [(&str, Vec<&str>); 4] = [
            ("ai-backend", AI.to_vec()),
            ("models-yaml", [AI, &["config models show"]].concat()),
            (
                "repo",
                vec![
                    "git",
                    "coverage",
                    "config scopes",
                    "worktrees rebase",
                    "worktrees push",
                    "ai jev route",
                    "ai jev verify-decision",
                ],
            ),
            (
                "instance",
                vec![
                    "atlassian jira",
                    "atlassian confluence",
                    "atlassian auth status",
                ],
            ),
        ];

        for (flag, owners) in &expected {
            let mut matched = 0;
            for (path, longs) in &all {
                let accepts = longs.iter().any(|l| l == flag);
                let should = under(path, owners);
                assert_eq!(
                    accepts,
                    should,
                    "`{path}` {} `--{flag}`, but the placement table says it {}",
                    if accepts { "accepts" } else { "rejects" },
                    if should {
                        "reads it"
                    } else {
                        "does not read it"
                    },
                );
                matched += usize::from(accepts);
            }
            assert!(matched > 0, "no command accepts `--{flag}`");
        }

        // `--profile` stays global: every leaf accepts it.
        for (path, longs) in &all {
            assert!(
                longs.iter().any(|l| l == "profile"),
                "`{path}` should accept the global `--profile`"
            );
        }

        // Every `global = true` flag declared anywhere in the (un-built) tree,
        // pinned by the exact node that declares it — so a new global, under
        // any name, cannot appear without being added here on purpose.
        use std::collections::BTreeSet;
        fn declared_globals(cmd: &clap::Command, path: &str, out: &mut BTreeSet<String>) {
            for arg in cmd.get_arguments().filter(|a| a.is_global_set()) {
                let long = arg.get_long().unwrap_or_else(|| arg.get_id().as_str());
                out.insert(format!("{path} --{long}"));
            }
            for sub in cmd.get_subcommands() {
                declared_globals(sub, &format!("{path} {}", sub.get_name()), out);
            }
        }
        let mut declared = BTreeSet::new();
        declared_globals(&Cli::command(), "omni-dev", &mut declared);
        let mut expected_globals: BTreeSet<String> = [
            "omni-dev --profile",
            "omni-dev git --repo",
            "omni-dev coverage --repo",
            "omni-dev config scopes --repo",
            // `RepoArg` is `global = true` even on this leaf (a no-op there).
            "omni-dev ai jev route --repo",
            "omni-dev ai jev verify-decision --repo",
            "omni-dev atlassian jira --instance",
            "omni-dev atlassian confluence --instance",
            "omni-dev atlassian auth status --instance",
            // Pre-#1778 subtree globals, scoped the same way.
            "omni-dev gmail --account",
            "omni-dev drive --account",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        if cfg!(unix) {
            // `RepoArg` is `global = true` even on these leaves (a no-op there).
            expected_globals.insert("omni-dev worktrees rebase --repo".to_string());
            expected_globals.insert("omni-dev worktrees push --repo".to_string());
        }
        assert_eq!(
            declared, expected_globals,
            "the set of `global = true` flags changed — apply the placement rule \
             (#1778) and update this list"
        );
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

    // ── propagate_profile_flag() tests ──
    //
    // These tests mutate `OMNI_DEV_PROFILE`, which `execute()` reads (via
    // `validate_active_profile`) in the `execute_routes_*` tests above. Those
    // hold `HOME_ENV_MUTEX` (through the Gmail/Drive `EnvGuard`s), so these
    // serialise on the same mutex — otherwise a profile set here makes a
    // concurrent routing test fail with "unknown profile".

    const PROFILE_VAR: &str = "OMNI_DEV_PROFILE";

    /// Locks the shared mutex and snapshots/restores every env var
    /// `propagate_profile_flag` may touch.
    struct ProfileEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: [(&'static str, Option<String>); 1],
    }

    impl ProfileEnvGuard {
        fn new() -> Self {
            let lock = crate::test_support::HOME_ENV_MUTEX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let names = [PROFILE_VAR];
            let saved = names.map(|n| (n, std::env::var(n).ok()));
            for (n, _) in &saved {
                std::env::remove_var(n);
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for ProfileEnvGuard {
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
    fn propagate_profile_flag_defaults_set_nothing() {
        let _g = ProfileEnvGuard::new();
        cli_with_defaults().propagate_profile_flag();
        assert!(std::env::var(PROFILE_VAR).is_err());
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
    fn propagate_profile_flag_sets_profile() {
        let _g = ProfileEnvGuard::new();
        let mut cli = cli_with_defaults();
        cli.profile = Some("work".to_string());
        cli.propagate_profile_flag();
        assert_eq!(std::env::var(PROFILE_VAR).ok().as_deref(), Some("work"));
    }

    #[test]
    fn propagate_profile_flag_profile_flag_beats_env_var() {
        let _g = ProfileEnvGuard::new();
        std::env::set_var(PROFILE_VAR, "personal");
        let mut cli = cli_with_defaults();
        cli.profile = Some("work".to_string());
        cli.propagate_profile_flag();
        assert_eq!(std::env::var(PROFILE_VAR).ok().as_deref(), Some("work"));
    }

    #[test]
    fn propagate_profile_flag_absent_profile_leaves_env_var() {
        let _g = ProfileEnvGuard::new();
        std::env::set_var(PROFILE_VAR, "personal");
        cli_with_defaults().propagate_profile_flag();
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
