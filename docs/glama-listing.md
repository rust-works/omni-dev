# Glama Listing

omni-dev-mcp is listed on Glama at <https://glama.ai/mcp/servers/rust-works/omni-dev>. Listing publication is driven by a per-commit Docker build configured through Glama's web admin page at <https://glama.ai/mcp/servers/rust-works/omni-dev/admin/dockerfile>.

This page is the canonical record of that configuration. The values that drive Glama's build (Build steps, CMD arguments, env-var schema, etc.) live only inside Glama's web form — they are **not** stored in this repo and would be lost if reconfigured. Treat this page as the source of truth and re-paste from here if the admin form ever needs to be reset.

## How the Glama Build Works

- Glama generates its own Dockerfile at build time from form fields on the admin page (base image, Node/Python versions, Build steps, CMD arguments).
- It does **not** consume [`docker/omni-dev-mcp.Dockerfile`](../docker/omni-dev-mcp.Dockerfile) from this repo. The committed Dockerfile is a local approximation for `docker build` testing only.
- The build is single-stage: the Rust toolchain, source clone, and final runtime all share one image. This is fatter than the local multi-stage Dockerfile but matches the shape Glama's template enforces.
- The runtime entrypoint is [`mcp-proxy`](https://github.com/sparfenyuk/mcp-proxy), which bridges stdio to SSE. The `omni-dev-mcp` binary itself speaks stdio MCP; mcp-proxy is needed because Glama's listing-check harness talks to servers over SSE.

## Admin Form Configuration

The admin page exposes the following fields. Settings below are the current values — keep this page in sync with the form.

| Field             | Value                |
|-------------------|----------------------|
| Base image        | `debian:trixie-slim` |
| Node.js version   | `26`                 |
| Python version    | `3.14`               |
| Pinned commit SHA | *per-release; see [Release Procedure](#release-procedure)* |
| Placeholder parameters | `{}`            |

### Build steps

Installs the Rust toolchain and compiles the `omni-dev-mcp` binary into `/usr/local/bin/`. Paste as a JSON array:

```json
[
  "apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev cmake build-essential && rm -rf /var/lib/apt/lists/*",
  "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal",
  ". \"$HOME/.cargo/env\" && cargo build --release --features mcp --bin omni-dev-mcp",
  "install -m 0755 target/release/omni-dev-mcp /usr/local/bin/omni-dev-mcp"
]
```

Notes:
- `pkg-config` + `libssl-dev` are needed for `openssl-sys` (transitive via reqwest's default native-tls feature).
- `cmake` + `build-essential` are needed for `libgit2-sys`, which builds libgit2 from vendored sources.
- The `mcp` cargo feature is required because `omni-dev-mcp` is gated behind it (see [`Cargo.toml`](../Cargo.toml) `required-features = ["mcp"]`).
- The `rustup` install uses `--profile minimal` to skip docs/rust-src/etc. and keep the image smaller.

### CMD arguments

Invokes the compiled binary under mcp-proxy:

```json
["mcp-proxy", "--", "omni-dev-mcp"]
```

The `--` separator is mandatory: mcp-proxy treats everything after `--` as the wrapped command. Without it (or without a command after it) mcp-proxy exits with `Error: No command specified`.

### Environment variables JSON schema

Declares the env vars the server understands. `required: []` is intentional — Glama's check only exercises `initialize` / `list_tools`, which the server answers without credentials. Tool calls that need credentials (Anthropic, OpenAI, Bedrock, Ollama, Atlassian, Datadog) fail at invocation time, not at startup.

```json
{
  "properties": {
    "ANTHROPIC_API_KEY": {
      "description": "Anthropic API key for the default AI backend",
      "type": "string"
    },
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": {
      "description": "Override the model identifier used when a Haiku-tier model is requested",
      "type": "string"
    },
    "ANTHROPIC_DEFAULT_OPUS_MODEL": {
      "description": "Override the model identifier used when an Opus-tier model is requested",
      "type": "string"
    },
    "ANTHROPIC_DEFAULT_SONNET_MODEL": {
      "description": "Override the model identifier used when a Sonnet-tier model is requested",
      "type": "string"
    },
    "ANTHROPIC_MODEL": {
      "description": "Override the default Anthropic model identifier (e.g. claude-opus-4-7)",
      "type": "string"
    },
    "ATLASSIAN_API_TOKEN": {
      "description": "Atlassian API token for Jira and Confluence tools",
      "type": "string"
    },
    "ATLASSIAN_EMAIL": {
      "description": "Atlassian account email associated with the API token",
      "type": "string"
    },
    "ATLASSIAN_INSTANCE_URL": {
      "description": "Atlassian instance base URL (e.g. https://your-org.atlassian.net)",
      "type": "string"
    },
    "AWS_ACCESS_KEY_ID": {
      "description": "AWS access key ID for the Bedrock backend (used by the AWS SDK)",
      "type": "string"
    },
    "AWS_REGION": {
      "description": "AWS region for the Bedrock backend (e.g. us-west-2)",
      "type": "string"
    },
    "AWS_SECRET_ACCESS_KEY": {
      "description": "AWS secret access key for the Bedrock backend (used by the AWS SDK)",
      "type": "string"
    },
    "AWS_SESSION_TOKEN": {
      "description": "AWS session token for the Bedrock backend when using temporary credentials",
      "type": "string"
    },
    "CLAUDE_API_KEY": {
      "description": "Alternative to ANTHROPIC_API_KEY; accepted by the default AI backend",
      "type": "string"
    },
    "CLAUDE_CODE_USE_BEDROCK": {
      "description": "Set to \"true\" to route AI calls through AWS Bedrock instead of the Anthropic API",
      "type": "string"
    },
    "DATADOG_API_KEY": {
      "description": "Override stored Datadog API key",
      "type": "string"
    },
    "DATADOG_API_URL": {
      "description": "Override site-derived URL for on-prem or proxied Datadog installs",
      "type": "string"
    },
    "DATADOG_APP_KEY": {
      "description": "Override stored Datadog application key",
      "type": "string"
    },
    "DATADOG_SITE": {
      "description": "Override stored Datadog site; defaults to datadoghq.com",
      "type": "string"
    },
    "OLLAMA_BASE_URL": {
      "description": "Base URL of the local Ollama or LM Studio server (e.g. http://localhost:11434)",
      "type": "string"
    },
    "OLLAMA_MODEL": {
      "description": "Model identifier to request from the Ollama-compatible server",
      "type": "string"
    },
    "OMNI_DEV_AI_BACKEND": {
      "description": "Select the AI backend explicitly. Options: claude-cli, ollama, openai, bedrock, or unset for the default Anthropic API",
      "type": "string"
    },
    "OMNI_DEV_CONFIG_DIR": {
      "description": "Override the directory used for omni-dev configuration (default: ~/.omni-dev)",
      "type": "string"
    },
    "OMNI_DEV_EDITOR": {
      "description": "Editor command used for interactive prompts (falls back to EDITOR, then a platform default)",
      "type": "string"
    },
    "OPENAI_API_KEY": {
      "description": "OpenAI API key, required when USE_OPENAI=true",
      "type": "string"
    },
    "OPENAI_AUTH_TOKEN": {
      "description": "Alternative to OPENAI_API_KEY; accepted by the OpenAI backend",
      "type": "string"
    },
    "RUST_LOG": {
      "description": "Tracing log filter (e.g. omni_dev=debug) for diagnosing issues",
      "type": "string"
    },
    "USE_OLLAMA": {
      "description": "Set to \"true\" to route AI calls through a local Ollama or LM Studio server",
      "type": "string"
    },
    "USE_OPENAI": {
      "description": "Set to \"true\" to route AI calls through the OpenAI Chat Completions API",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

When the codebase grows a new env var that a Glama user would plausibly set, update this schema *and* re-paste it into the admin form.

## Release Procedure

The Glama build is pinned to a specific commit SHA and only rebuilds when that SHA is bumped. After each omni-dev release, point Glama at the release commit so the listing reflects the new version.

This is a manual web-UI step; there is no API or CI integration.

1. Open the Dockerfile admin page: <https://glama.ai/mcp/servers/rust-works/omni-dev/admin/dockerfile>

2. Get the release commit SHA:
   ```bash
   git rev-parse --short vX.Y.Z
   ```

3. Paste it into the **Pinned commit SHA** field and **Save**. Glama re-renders its generated Dockerfile against the new commit.

4. Trigger a **build** from the admin page. The build clones the pinned commit, runs the Build steps (Rust install + `cargo build`), and starts `omni-dev-mcp` under `mcp-proxy`. Wait for it to complete and verify the build log is green.

5. Once the build succeeds, trigger a **release** from the same page. This promotes the new image to the public listing.

## Local Verification

[`docker/omni-dev-mcp.Dockerfile`](../docker/omni-dev-mcp.Dockerfile) in this repo is a multi-stage approximation of what Glama builds. It is **not** byte-identical to Glama's generated Dockerfile (different stage layout, no Node/uv tooling pre-installed), but it's good enough to verify the binary starts and responds to MCP introspection.

```bash
docker build -t omni-dev-mcp -f docker/omni-dev-mcp.Dockerfile .
docker run -it --rm -e MCP_PROXY_DEBUG=true omni-dev-mcp
```

For a closer match to Glama's environment, build the local Dockerfile with the single-stage Glama shape instead — but in practice the multi-stage local image catches the same failure modes (missing build deps, broken cargo features, binary panicking on startup).

## Troubleshooting

| Symptom in Glama build log | Likely cause |
|---|---|
| `Error: No command specified` from mcp-proxy | **CMD arguments** field is empty or missing the wrapped binary after `--`. |
| `omni-dev-mcp: command not found` | **Build steps** didn't install the binary into `PATH`. Check the `install -m 0755 target/release/...` line. |
| `error: failed to run custom build command for openssl-sys` | **Build steps** didn't install `pkg-config` + `libssl-dev`. |
| `error: failed to run custom build command for libgit2-sys` | **Build steps** didn't install `cmake` + `build-essential`. |
| `error: feature edition2024 is required` | The default rust toolchain shipped in the build is too old; pin a newer toolchain in the `rustup` install step. |
| Build succeeds but introspection times out | The binary panics at startup. Try running `docker run` locally with `RUST_LOG=omni_dev=debug` to see the panic message. |
