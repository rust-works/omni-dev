//! Per-command AI backend flags (`--ai-backend`, `--model`, `--beta-header`,
//! the `--claude-cli-*` escape hatches, and `--models-yaml`).
//!
//! These used to be `global = true` on the root [`Cli`](crate::cli::Cli), so
//! every subcommand accepted them and all but a handful silently ignored them
//! (#1778). They are now declared only on the commands that read them.
//!
//! **Placement rule:** a flag attaches to the **narrowest node whose entire
//! subtree reads it** — subtree-`global = true` there (the `gmail --account`
//! precedent), or `#[command(flatten)]`ed onto individual leaves when the
//! readers are scattered, as the AI commands are. clap then rejects a flag on
//! a command that would ignore it, and `--help` is accurate by construction.
//!
//! The values are still forwarded to environment variables by [`apply`]
//! rather than threaded as parameters: `resolve_backend`/`resolve_model`,
//! preflight, `ClaudeCliAiClient`, and the MCP binary all read the environment,
//! so none of them changes. Each owning command calls [`apply`] as the first
//! line of its `execute()`, before preflight and before building a client.
//!
//! [`apply`]: AiBackendArgs::apply

use clap::Args;

use crate::claude::backend::AiBackend;

/// `--models-yaml`: the one AI flag also read by a non-AI command
/// (`config models show`), so it is its own group.
#[derive(Args, Debug, Clone, Default)]
pub struct ModelsYamlArg {
    /// Path to a single user-side `models.yaml` that short-circuits the
    /// standard `./.omni-dev/models.yaml` and `~/.omni-dev/models.yaml`
    /// lookup. The file is still merged over the embedded catalog.
    /// Equivalent to setting `OMNI_DEV_MODELS_YAML`.
    #[arg(long, value_name = "PATH")]
    pub models_yaml: Option<std::path::PathBuf>,
}

impl ModelsYamlArg {
    /// Forwards `--models-yaml` to `OMNI_DEV_MODELS_YAML`, which the model
    /// registry reads on first use. Must run before `get_model_registry()`.
    pub fn apply(&self) {
        if let Some(path) = &self.models_yaml {
            std::env::set_var("OMNI_DEV_MODELS_YAML", path);
        }
    }
}

/// The AI backend flags, flattened onto every command that builds an AI
/// client (`git commit message {twiddle,check,staged}`, `git branch create pr`,
/// `ai chat`).
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "AI backend")]
pub struct AiBackendArgs {
    /// Selects the AI backend used by this command.
    ///
    /// Overrides the `OMNI_DEV_AI_BACKEND` environment variable and the
    /// legacy `USE_OPENAI`/`USE_OLLAMA`/`CLAUDE_CODE_USE_BEDROCK` variables
    /// (`default` forces the direct Anthropic API even when they are set).
    #[arg(long, value_enum)]
    pub ai_backend: Option<AiBackend>,

    /// AI model to use for this command.
    ///
    /// Highest-precedence model selector: it overrides `OMNI_DEV_MODEL` and
    /// every per-backend model variable (`CLAUDE_MODEL`, `CLAUDE_CODE_MODEL`,
    /// `ANTHROPIC_MODEL`, `OPENAI_MODEL`, `OLLAMA_MODEL`). Equivalent to
    /// setting `OMNI_DEV_MODEL`.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Beta header to send with AI API requests (format: key:value).
    ///
    /// Only sent if the model supports it in the model registry. Equivalent
    /// to setting `OMNI_DEV_BETA_HEADER`. Ignored when `--ai-backend` is
    /// `claude-cli` (the CLI negotiates betas itself).
    #[arg(long, value_name = "KEY:VALUE")]
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
    #[arg(long)]
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
    #[arg(long)]
    pub claude_cli_allow_mcp: bool,

    /// Per-invocation spending cap in USD for the `claude-cli` backend.
    ///
    /// Forwarded to `claude -p --max-budget-usd`. When the nested session
    /// exceeds this budget it aborts rather than running away with cost.
    /// Equivalent to setting `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD`.
    ///
    /// Ignored when `--ai-backend` is not `claude-cli`.
    #[arg(long, value_name = "AMOUNT")]
    pub claude_cli_max_budget_usd: Option<f64>,

    /// `--models-yaml`, shared with `config models show`.
    #[command(flatten)]
    pub models_yaml: ModelsYamlArg,
}

impl AiBackendArgs {
    /// Forwards the flags to the env vars that the backend/model resolvers,
    /// preflight, and the `claude-cli` client read. Setting the env vars
    /// (rather than threading extra arguments through every factory) keeps
    /// those signatures stable and shared with the MCP binary.
    pub fn apply(&self) {
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

        self.models_yaml.apply();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    // ── parsing: the flags live after the AI leaf, and nowhere else ──

    const TWIDDLE: [&str; 5] = ["omni-dev", "git", "commit", "message", "twiddle"];

    fn parse_twiddle(extra: &[&str]) -> AiBackendArgs {
        let argv: Vec<&str> = TWIDDLE.iter().chain(extra).copied().collect();
        let cli = Cli::try_parse_from(argv).unwrap();
        let crate::cli::Commands::Git(git) = cli.command else {
            panic!("expected a git command");
        };
        let crate::cli::git::GitSubcommands::Commit(commit) = git.command else {
            panic!("expected git commit");
        };
        let crate::cli::git::CommitSubcommands::Message(message) = commit.command;
        let crate::cli::git::MessageSubcommands::Twiddle(twiddle) = message.command else {
            panic!("expected twiddle");
        };
        twiddle.ai
    }

    #[test]
    fn parses_every_flag_after_the_leaf() {
        let ai = parse_twiddle(&[
            "--ai-backend",
            "claude-cli",
            "--model",
            "claude-opus-4-6",
            "--beta-header",
            "anthropic-beta:output-128k-2025-02-19",
            "--claude-cli-allow-tools",
            "--claude-cli-allow-mcp",
            "--claude-cli-max-budget-usd",
            "0.50",
            "--models-yaml",
            "/tmp/custom-models.yaml",
        ]);
        assert_eq!(ai.ai_backend, Some(AiBackend::ClaudeCli));
        assert_eq!(ai.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(
            ai.beta_header.as_deref(),
            Some("anthropic-beta:output-128k-2025-02-19")
        );
        assert!(ai.claude_cli_allow_tools);
        assert!(ai.claude_cli_allow_mcp);
        assert_eq!(ai.claude_cli_max_budget_usd, Some(0.50));
        assert_eq!(
            ai.models_yaml.models_yaml.as_deref(),
            Some(std::path::Path::new("/tmp/custom-models.yaml"))
        );
    }

    #[test]
    fn absent_flags_are_default() {
        let ai = parse_twiddle(&[]);
        assert!(ai.ai_backend.is_none());
        assert!(ai.model.is_none());
        assert!(ai.beta_header.is_none());
        assert!(!ai.claude_cli_allow_tools);
        assert!(!ai.claude_cli_allow_mcp);
        assert!(ai.claude_cli_max_budget_usd.is_none());
        assert!(ai.models_yaml.models_yaml.is_none());
    }

    #[test]
    fn parses_each_ai_backend_value() {
        for (value, expected) in [
            ("default", AiBackend::Default),
            ("claude-cli", AiBackend::ClaudeCli),
            ("openai", AiBackend::OpenAi),
            ("ollama", AiBackend::Ollama),
            ("bedrock", AiBackend::Bedrock),
        ] {
            let ai = parse_twiddle(&["--ai-backend", value]);
            assert_eq!(ai.ai_backend, Some(expected), "value {value}");
        }
    }

    #[test]
    fn max_budget_usd_rejects_non_numeric() {
        let argv: Vec<&str> = TWIDDLE
            .iter()
            .chain(&["--claude-cli-max-budget-usd", "cheap"])
            .copied()
            .collect();
        let Err(err) = Cli::try_parse_from(argv) else {
            panic!("expected parse error for non-numeric budget");
        };
        assert!(err.to_string().contains("invalid"));
    }

    /// Every AI leaf accepts the group.
    #[test]
    fn every_ai_command_accepts_the_flags() {
        for leaf in [
            &["git", "commit", "message", "twiddle"][..],
            &["git", "commit", "message", "check"],
            &["git", "commit", "message", "staged"],
            &["git", "branch", "create", "pr"],
            &["ai", "chat"],
        ] {
            let argv: Vec<&str> = std::iter::once("omni-dev")
                .chain(leaf.iter().copied())
                .chain(["--ai-backend", "claude-cli", "--model", "m"])
                .collect();
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "`{}` should accept the AI backend flags",
                leaf.join(" ")
            );
        }
    }

    /// The pre-#1778 placement — before the subcommand — no longer parses:
    /// there is no root-level AI flag left to bind it.
    #[test]
    fn flags_before_the_subcommand_are_rejected() {
        for flag in [
            &["--model", "claude-opus-4-6"][..],
            &["--ai-backend", "claude-cli"],
            &["--beta-header", "k:v"],
            &["--claude-cli-allow-tools"],
            &["--models-yaml", "/tmp/m.yaml"],
        ] {
            let argv: Vec<&str> = std::iter::once("omni-dev")
                .chain(flag.iter().copied())
                .chain(TWIDDLE[1..].iter().copied())
                .collect();
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "`{}` before the subcommand must be rejected",
                flag[0]
            );
        }
    }

    /// Regression test for #1778: `ai jev` never builds an AI client, so it
    /// must reject the AI backend flags instead of silently ignoring them.
    #[test]
    fn jev_rejects_ai_backend_flags() {
        for flag in [
            &["--claude-cli-allow-tools"][..],
            &["--claude-cli-allow-mcp"],
            &["--claude-cli-max-budget-usd", "1"],
            &["--ai-backend", "claude-cli"],
            &["--model", "claude-opus-4-6"],
            &["--beta-header", "k:v"],
            &["--models-yaml", "/tmp/m.yaml"],
        ] {
            let argv: Vec<&str> = [
                "omni-dev",
                "ai",
                "jev",
                "noul",
                "--instructions",
                "i",
                "--true-means",
                "t",
                "--false-means",
                "f",
            ]
            .into_iter()
            .chain(flag.iter().copied())
            .collect();
            let Err(err) = Cli::try_parse_from(&argv) else {
                panic!("`ai jev noul {}` must be rejected", flag[0]);
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "`{}`: {err}",
                flag[0]
            );
        }
    }

    /// `config models show` reads the registry, so it takes `--models-yaml`
    /// — and only that: it never builds an AI client.
    #[test]
    fn config_models_show_takes_only_models_yaml() {
        assert!(Cli::try_parse_from([
            "omni-dev",
            "config",
            "models",
            "show",
            "--models-yaml",
            "/tmp/m.yaml",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "omni-dev",
            "config",
            "models",
            "show",
            "--ai-backend",
            "claude-cli",
        ])
        .is_err());
    }

    /// Grep guard: the files under `src/cli/` that build an AI client must be
    /// exactly the files that flatten [`AiBackendArgs`]. A new AI command that
    /// forgets the flags (so `--model` is rejected where it should work), or a
    /// non-AI command that gains them (so they are silently ignored again,
    /// #1778), fails here.
    #[test]
    fn ai_client_builders_are_exactly_the_ai_backend_args_owners() {
        use std::collections::BTreeSet;
        use std::path::{Path, PathBuf};

        fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let cli_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli");
        let mut files = Vec::new();
        rust_files(&cli_dir, &mut files);

        let mut builders = BTreeSet::new();
        let mut owners = BTreeSet::new();
        for path in files {
            let rel = path
                .strip_prefix(&cli_dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "ai_backend_args.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let code = source
                .lines()
                .map(str::trim_start)
                .filter(|l| !l.starts_with("//"));
            for line in code {
                if line.contains("create_default_claude_client(") {
                    builders.insert(rel.clone());
                }
                // A field declaration (`pub ai: …::AiBackendArgs,`), not a
                // struct-literal initializer (`…::AiBackendArgs::default()`).
                if line.starts_with("pub ") && line.ends_with("AiBackendArgs,") {
                    owners.insert(rel.clone());
                }
            }
        }

        assert!(!builders.is_empty(), "found no AI client builders");
        assert_eq!(
            builders, owners,
            "files calling `create_default_claude_client(` must be exactly the \
             files that flatten `AiBackendArgs` (placement rule, #1778)"
        );
    }

    // ── apply() ──
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

    /// Locks the shared mutex and snapshots/restores every env var `apply`
    /// may touch.
    struct AiEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: [(&'static str, Option<String>); 7],
    }

    impl AiEnvGuard {
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
            ];
            let saved = names.map(|n| (n, std::env::var(n).ok()));
            for (n, _) in &saved {
                std::env::remove_var(n);
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for AiEnvGuard {
        fn drop(&mut self) {
            for (n, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(n, v),
                    None => std::env::remove_var(n),
                }
            }
        }
    }

    #[test]
    fn apply_defaults_set_nothing() {
        let _g = AiEnvGuard::new();
        AiBackendArgs::default().apply();
        for var in [
            BACKEND_VAR,
            MODEL_VAR,
            BETA_HEADER_VAR,
            ALLOW_TOOLS_VAR,
            ALLOW_MCP_VAR,
            MAX_BUDGET_VAR,
            MODELS_YAML_VAR,
        ] {
            assert!(std::env::var(var).is_err(), "{var} should be unset");
        }
    }

    #[test]
    fn apply_sets_ai_backend() {
        let _g = AiEnvGuard::new();
        for (backend, expected) in [
            (AiBackend::ClaudeCli, "claude-cli"),
            (AiBackend::OpenAi, "openai"),
            (AiBackend::Ollama, "ollama"),
            (AiBackend::Bedrock, "bedrock"),
        ] {
            AiBackendArgs {
                ai_backend: Some(backend),
                ..Default::default()
            }
            .apply();
            assert_eq!(std::env::var(BACKEND_VAR).ok().as_deref(), Some(expected));
        }
    }

    #[test]
    fn apply_default_backend_overrides_env_var() {
        // `--ai-backend default` must *set* the env var (not remove it) so it
        // decisively overrides both a pre-set backend and the legacy USE_*
        // flags (#1118).
        let _g = AiEnvGuard::new();
        std::env::set_var(BACKEND_VAR, "claude-cli");
        AiBackendArgs {
            ai_backend: Some(AiBackend::Default),
            ..Default::default()
        }
        .apply();
        assert_eq!(std::env::var(BACKEND_VAR).ok().as_deref(), Some("default"));
    }

    #[test]
    fn apply_sets_model() {
        let _g = AiEnvGuard::new();
        AiBackendArgs {
            model: Some("claude-opus-4-6".to_string()),
            ..Default::default()
        }
        .apply();
        assert_eq!(
            std::env::var(MODEL_VAR).ok().as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[test]
    fn apply_sets_beta_header() {
        let _g = AiEnvGuard::new();
        AiBackendArgs {
            beta_header: Some("anthropic-beta:output-128k-2025-02-19".to_string()),
            ..Default::default()
        }
        .apply();
        assert_eq!(
            std::env::var(BETA_HEADER_VAR).ok().as_deref(),
            Some("anthropic-beta:output-128k-2025-02-19")
        );
    }

    #[test]
    fn apply_sets_allow_tools() {
        let _g = AiEnvGuard::new();
        AiBackendArgs {
            claude_cli_allow_tools: true,
            ..Default::default()
        }
        .apply();
        assert_eq!(std::env::var(ALLOW_TOOLS_VAR).ok().as_deref(), Some("true"));
        // The flag export is recorded for WARN provenance (issue #1143). The
        // registry is additive-only, so this assertion is order-independent.
        assert!(crate::utils::settings::exported_by_cli_flag(
            ALLOW_TOOLS_VAR
        ));
    }

    #[test]
    fn apply_sets_allow_mcp() {
        let _g = AiEnvGuard::new();
        AiBackendArgs {
            claude_cli_allow_mcp: true,
            ..Default::default()
        }
        .apply();
        assert_eq!(std::env::var(ALLOW_MCP_VAR).ok().as_deref(), Some("true"));
        assert!(crate::utils::settings::exported_by_cli_flag(ALLOW_MCP_VAR));
    }

    #[test]
    fn apply_sets_max_budget_usd() {
        let _g = AiEnvGuard::new();
        AiBackendArgs {
            claude_cli_max_budget_usd: Some(1.5),
            ..Default::default()
        }
        .apply();
        assert_eq!(std::env::var(MAX_BUDGET_VAR).ok().as_deref(), Some("1.5"));
    }

    #[test]
    fn apply_sets_models_yaml() {
        let _g = AiEnvGuard::new();
        AiBackendArgs {
            models_yaml: ModelsYamlArg {
                models_yaml: Some(std::path::PathBuf::from("/tmp/custom-models.yaml")),
            },
            ..Default::default()
        }
        .apply();
        assert_eq!(
            std::env::var(MODELS_YAML_VAR).ok().as_deref(),
            Some("/tmp/custom-models.yaml")
        );
    }

    #[test]
    fn apply_independent_flags_compose() {
        let _g = AiEnvGuard::new();
        AiBackendArgs {
            ai_backend: Some(AiBackend::ClaudeCli),
            claude_cli_allow_tools: true,
            claude_cli_allow_mcp: true,
            claude_cli_max_budget_usd: Some(0.25),
            ..Default::default()
        }
        .apply();
        assert_eq!(
            std::env::var(BACKEND_VAR).ok().as_deref(),
            Some("claude-cli")
        );
        assert_eq!(std::env::var(ALLOW_TOOLS_VAR).ok().as_deref(), Some("true"));
        assert_eq!(std::env::var(ALLOW_MCP_VAR).ok().as_deref(), Some("true"));
        assert_eq!(std::env::var(MAX_BUDGET_VAR).ok().as_deref(), Some("0.25"));
    }
}
