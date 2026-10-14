//! `omni-dev ai gateway` — the Anthropic Messages gateway (#2226).
//!
//! `serve` runs [`crate::gateway::GatewayServer`] in the foreground (the
//! daemon's `anthropic-gateway` service hosts the same server); `env` prints
//! the `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` a client needs; `fields`
//! prints the field policy. See `docs/anthropic-gateway.md`.

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

use crate::gateway::config::{port_from_env, resolve_token, token_path};
use crate::gateway::{
    policy, ConfigOverrides, Flavor, GatewayConfig, GatewayServer, UpstreamProtocol,
};

/// Anthropic Messages gateway in front of OpenAI-protocol upstreams.
#[derive(Parser)]
#[command(
    long_about = "Anthropic Messages gateway in front of OpenAI-protocol upstreams.\n\nServes \
`/v1/messages` (streaming and not), `/v1/messages/count_tokens` and `/v1/models` on \
127.0.0.1, translating to an OpenAI Chat Completions or Responses upstream (OpenAI, Ollama, \
LM Studio, vLLM, OpenRouter, Fireworks), passing through to an Anthropic-compatible one, \
or invoking native Bedrock Claude with bearer authentication. \
Point Claude Code or an Anthropic SDK at it with the values `omni-dev ai gateway env` prints. \
Configure upstreams and optional model-based routing with OMNI_DEV_GATEWAY_* variables (settings.json and --profile \
apply); see docs/anthropic-gateway.md."
)]
pub struct GatewayCommand {
    /// The gateway subcommand to execute.
    #[command(subcommand)]
    pub command: GatewaySubcommands,
}

/// Gateway subcommands.
#[derive(Subcommand)]
pub enum GatewaySubcommands {
    /// Runs the gateway in the foreground until interrupted.
    Serve(ServeCommand),
    /// Prints the environment a client needs to use the gateway.
    Env(EnvCommand),
    /// Prints how each Anthropic request field is handled.
    Fields(FieldsCommand),
}

impl GatewayCommand {
    /// Executes the gateway command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            GatewaySubcommands::Serve(cmd) => cmd.execute().await,
            GatewaySubcommands::Env(cmd) => cmd.execute(),
            GatewaySubcommands::Fields(cmd) => cmd.execute(),
        }
    }
}

/// Runs the gateway in the foreground until interrupted.
///
/// Flags override the matching `OMNI_DEV_GATEWAY_*` variables. The gateway
/// token is never taken from the command line: it comes from
/// OMNI_DEV_GATEWAY_TOKEN (or its _FILE/_COMMAND companion), else the
/// persisted token file.
#[derive(Parser)]
pub struct ServeCommand {
    /// Port on 127.0.0.1 (`0` picks a free one). Default: OMNI_DEV_GATEWAY_PORT, else 9997.
    #[arg(long)]
    pub port: Option<u16>,

    /// Upstream base URL, e.g. `https://api.fireworks.ai/inference` or `http://localhost:11434`.
    #[arg(long, value_name = "URL")]
    pub upstream_url: Option<String>,

    /// Protocol the upstream speaks.
    #[arg(long, value_enum)]
    pub upstream_protocol: Option<UpstreamProtocol>,

    /// Upstream provider family, for its quirks.
    #[arg(long, value_enum)]
    pub flavor: Option<Flavor>,

    /// Default upstream model, used for client model names the map does not cover.
    #[arg(long)]
    pub model: Option<String>,

    /// Model mapping `PATTERN=MODEL` (`*` globs, e.g. `claude-haiku-*=small`); repeatable.
    #[arg(long = "map", value_name = "PATTERN=MODEL")]
    pub model_map: Vec<String>,
}

impl ServeCommand {
    fn overrides(&self) -> ConfigOverrides {
        ConfigOverrides {
            port: self.port,
            upstream_url: self.upstream_url.clone(),
            protocol: self.upstream_protocol,
            flavor: self.flavor,
            model: self.model.clone(),
            model_map: self.model_map.clone(),
        }
    }

    /// Executes the serve command.
    pub async fn execute(self) -> Result<()> {
        let config = GatewayConfig::from_env(&self.overrides())?;
        let path = token_path()?;
        let token = resolve_token(&path)?;
        let summary = config.describe();
        let server = GatewayServer::start(config, token)?;
        eprintln!(
            "omni-dev Anthropic gateway listening on {}\n  {summary}\n  token: OMNI_DEV_GATEWAY_TOKEN if set, else {}\n\
             Run `omni-dev ai gateway env` for the client settings; Ctrl-C stops.",
            GatewayConfig::base_url_for(server.port()),
            path.display()
        );
        server.wait().await
    }
}

/// The output format of `env`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum EnvFormat {
    /// `export NAME=value` lines for a POSIX shell.
    #[default]
    Shell,
    /// A `{"env": {...}}` object for Claude Code's `settings.json`.
    Json,
}

/// Prints the environment a client needs to use the gateway.
///
/// Prints `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` (the gateway token,
/// created on first use). Claude Code and the Anthropic SDKs read both.
#[derive(Parser)]
pub struct EnvCommand {
    /// Port to print (default: OMNI_DEV_GATEWAY_PORT, else 9997).
    #[arg(long)]
    pub port: Option<u16>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = EnvFormat::Shell)]
    pub format: EnvFormat,
}

impl EnvCommand {
    /// Executes the env command.
    pub fn execute(self) -> Result<()> {
        let env = crate::utils::settings::SettingsEnv::load();
        let port = match self.port {
            Some(port) => port,
            None => port_from_env(&env)?,
        };
        let token = resolve_token(&token_path()?)?;
        run_env(port, &token, self.format, &mut std::io::stdout().lock())
    }
}

/// Writes the client environment for `port` and `token`.
pub(crate) fn run_env(
    port: u16,
    token: &str,
    format: EnvFormat,
    out: &mut impl Write,
) -> Result<()> {
    let base_url = GatewayConfig::base_url_for(port);
    match format {
        EnvFormat::Shell => {
            writeln!(out, "export ANTHROPIC_BASE_URL={base_url}")?;
            writeln!(out, "export ANTHROPIC_AUTH_TOKEN={token}")?;
        }
        EnvFormat::Json => {
            let value =
                json!({"env": {"ANTHROPIC_BASE_URL": base_url, "ANTHROPIC_AUTH_TOKEN": token}});
            writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
        }
    }
    Ok(())
}

/// Prints how each Anthropic request field, content block, tool field, stop
/// reason and stream delta is handled (translated, ignored or rejected).
#[derive(Parser)]
pub struct FieldsCommand {}

impl FieldsCommand {
    /// Executes the fields command.
    pub fn execute(self) -> Result<()> {
        run_fields(&mut std::io::stdout().lock())
    }
}

/// Writes the field policy as Markdown.
pub(crate) fn run_fields(out: &mut impl Write) -> Result<()> {
    write!(out, "{}", policy::render_markdown())?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    #[test]
    fn env_prints_shell_and_json() {
        let mut shell = Vec::new();
        run_env(9997, "tok", EnvFormat::Shell, &mut shell).unwrap();
        assert_eq!(
            String::from_utf8(shell).unwrap(),
            "export ANTHROPIC_BASE_URL=http://127.0.0.1:9997\nexport ANTHROPIC_AUTH_TOKEN=tok\n"
        );
        let mut json_out = Vec::new();
        run_env(1, "t", EnvFormat::Json, &mut json_out).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:1");
        assert_eq!(value["env"]["ANTHROPIC_AUTH_TOKEN"], "t");
    }

    #[test]
    fn fields_prints_the_policy() {
        let mut out = Vec::new();
        run_fields(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("| `container` | rejected |"));
        assert!(text.contains("#### Content blocks"));
    }

    #[test]
    fn serve_flags_become_overrides() {
        let cmd = ServeCommand::try_parse_from([
            "serve",
            "--port",
            "0",
            "--upstream-url",
            "http://localhost:11434",
            "--upstream-protocol",
            "responses",
            "--flavor",
            "openai",
            "--model",
            "m",
            "--map",
            "a=b",
            "--map",
            "c=d",
        ])
        .unwrap();
        let overrides = cmd.overrides();
        assert_eq!(overrides.port, Some(0));
        assert_eq!(overrides.protocol, Some(UpstreamProtocol::Responses));
        assert_eq!(overrides.flavor, Some(Flavor::Openai));
        assert_eq!(overrides.model_map, vec!["a=b", "c=d"]);
        let config = GatewayConfig::from_env_with(&MapEnv::new(), &overrides).unwrap();
        let summary = config.describe();
        assert!(summary.contains("http://localhost:11434"));
        assert!(summary.contains("responses protocol"));
        assert!(summary.contains("2 mapping rule(s)"));
    }

    #[test]
    fn the_token_is_not_a_flag() {
        assert!(ServeCommand::try_parse_from(["serve", "--token", "x"]).is_err());
        assert!(EnvCommand::try_parse_from(["env", "--format", "json"]).is_ok());
    }
}
