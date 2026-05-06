# Architecture

This document describes the high-level design of omni-dev. It is intended to help developers quickly build a mental model of the codebase.

## System overview

omni-dev is a developer toolkit that pairs AI-powered Git commit analysis with read/write access to Atlassian and Datadog. It ships as two binaries:

- **`omni-dev`** — the primary CLI: git commit workflows, PR creation, JIRA/Confluence integration, Datadog queries, and AI diagnostics.
- **`omni-dev-mcp`** — an optional MCP server gated behind the `mcp` Cargo feature that exposes the same surface to AI assistants over stdio. See [ADR-0021](docs/adrs/adr-0021.md).

The git/AI workflows are:

- **twiddle** — Generate or improve commit messages using AI analysis of diffs
- **check** — Validate existing commit messages against project guidelines
- **create-pr** — Generate pull request titles and descriptions from branch commits

All AI-powered workflows share the same pipeline: parse CLI arguments, read git repository state, build a structured YAML representation, construct a token-budget-aware prompt, call an AI provider, parse the response, and apply or display the result. The Atlassian and Datadog command trees are thinner — they wrap typed REST clients and emit YAML/table/json/yamls/jsonl output without invoking AI.

## Module map

```
src/
├── main.rs              `omni-dev` entry point: tracing, Cli::parse(), execute()
├── mcp_server.rs        `omni-dev-mcp` entry point (gated by `mcp` feature)
├── lib.rs               Public module exports and VERSION constant
├── cli.rs               Clap command hierarchy root + global flags
├── cli/
│   ├── git/             twiddle / check / view / amend / info / create_pr
│   ├── ai/              chat, claude history, claude skills, claude cli model
│   ├── atlassian/       jira/* and confluence/* command trees, convert
│   ├── datadog/         metrics / monitor / dashboard / logs / events / slo / hosts / downtime / auth
│   ├── config.rs        Model registry display
│   ├── commands.rs      Command template generators (`commands generate ...`)
│   └── help.rs          Help system (`help-all`)
├── mcp/                 (mcp feature) MCP server runtime
│   ├── server.rs        rmcp ServerHandler implementation
│   ├── runtime.rs       stdio transport bootstrapping
│   ├── git_tools.rs     git_view_commits, git_branch_info, git_check_commits, git_twiddle_commits, git_create_pr
│   ├── jira_core_tools.rs / jira_tools.rs   ~28 jira_* tools
│   ├── confluence_tools.rs                  13 confluence_* tools
│   ├── atlassian_tools.rs                   atlassian_convert (offline JFM ↔ ADF)
│   ├── datadog_tools.rs                     14 datadog_* tools
│   ├── ai_tools.rs                          ai_chat, claude_skills_*
│   ├── config_tools.rs                      config_models_show, atlassian_auth_status
│   ├── resources.rs                         git://, jira://, confluence://, omni-dev://specs/{name}
│   ├── specs.rs                             include_str! reference specs (jfm)
│   ├── output_file.rs                       Off-context write helper for read tools
│   └── truncate.rs / validate.rs / cancel.rs / error.rs
├── claude/
│   ├── ai.rs            AiClient trait and metadata types
│   ├── ai/
│   │   ├── claude.rs    Anthropic API implementation
│   │   ├── claude_cli.rs Subprocess `claude -p` backend with sandbox + budget cap
│   │   ├── openai.rs    OpenAI/Ollama implementation
│   │   └── bedrock.rs   AWS Bedrock implementation
│   ├── client.rs        Backend dispatch (create_default_claude_client) + orchestrator
│   ├── prompts.rs       System and user prompt templates
│   ├── model_config.rs  Model registry with fuzzy matching
│   ├── token_budget.rs  Token estimation and budget validation
│   ├── batch.rs         Token-budget-aware commit batching
│   ├── error.rs         ClaudeError types
│   └── context/         discovery / branch / files / patterns
├── atlassian/           Typed Atlassian client + ADF/JFM bidirectional conversion
│   ├── client.rs        REST client (JIRA + Confluence)
│   ├── adf/ jfm/        ADF and JIRA-Flavoured Markdown types
│   ├── custom_fields.rs Custom-field overrides (`--set-field` / assignee / reporter)
│   └── ...              issues, sprints, boards, links, watchers, worklogs, attachments
├── datadog/             Typed Datadog client + per-endpoint API façades
│   ├── client.rs        Auth, 429 handling, X-RateLimit-* surfacing
│   ├── metrics_api.rs / monitors_api.rs / dashboards_api.rs / logs_api.rs / events_api.rs / slo_api.rs / hosts_api.rs / downtimes_api.rs / metrics_catalog_api.rs
│   └── types.rs / time.rs
├── data/                RepositoryView, AmendmentFile, CheckReport, ProjectContext, YAML utils
├── git/                 GitRepository (git2 wrapper), CommitInfo, AmendmentHandler, RemoteInfo
├── utils/
│   ├── settings.rs      Settings loading (env vars → ~/.omni-dev/settings.json)
│   ├── preflight.rs     AI credential / GitHub CLI / Atlassian / Datadog preflight
│   ├── ai_scratch.rs    `~/.cache/omni-dev/ai-scratch/` management
│   └── general.rs       General utilities
└── templates/
    ├── models.yaml                  AI model specifications
    └── default-commit-guidelines.md Embedded default guidelines
```

### Module responsibilities

**`claude/`** — AI integration layer. Contains the provider abstraction (`AiClient` trait), the orchestration logic (`ClaudeClient`), prompt engineering, token budget management, and project context discovery. This is the largest module.

**`cli/`** — Command-line interface. Each command is a `#[derive(Parser)]` struct with an `execute()` method. Commands construct a `RepositoryView`, delegate to `ClaudeClient` methods, and handle output formatting.

**`data/`** — Shared data structures. `RepositoryView` is the standard git state representation; `RepositoryViewForAI` adds full diff content. Amendment and check result types live here. All types derive `Serialize`/`Deserialize` for YAML exchange.

**`git/`** — Git operations via the `git2` crate. `GitRepository` wraps `git2::Repository` with higher-level methods for commit enumeration, diff generation, and working directory status. `AmendmentHandler` applies message changes through `git commit --amend` or interactive rebase.

**`utils/`** — Cross-cutting utilities. Settings resolution, preflight credential checks, and AI scratch directory management.

**`atlassian/`** — Typed JIRA + Confluence REST client and the bidirectional ADF ↔ JFM (JIRA-Flavoured Markdown) conversion layer. The CLI subcommands and the MCP `jira_*` / `confluence_*` tools are thin wrappers over this client; both paths share `custom_fields.rs` for typed assignee/reporter/`set_field` handling. See [ADR-0020](docs/adrs/adr-0020.md) for the ADF/JFM design.

**`datadog/`** — Typed Datadog v1/v2 client. Each endpoint family has its own façade (`monitors_api.rs`, `dashboards_api.rs`, etc.) emitting strongly-typed responses. The base `DatadogClient` handles 429 with `Retry-After` / `X-RateLimit-Reset` and surfaces rate-limit headers in error output.

**`mcp/`** *(feature `mcp`)* — MCP server runtime built on `rmcp`. Each tool family lives in its own module; tools are registered via the `#[tool]` macro and the per-domain `tool_router()` builders in `OmniDevServer::new()`. Tools build a fresh client per invocation so credential changes take effect without restart.

### AI backend dispatch

`src/claude/client.rs::create_default_claude_client` selects an `AiClient` implementation in this order:

1. `OMNI_DEV_AI_BACKEND=claude-cli` (or `--ai-backend claude-cli`) → `ClaudeCliAiClient` — shells out to `claude -p` in a sandbox (tools off, MCP off, settings skipped, fresh temp cwd, scrubbed env). Honours escape hatches `OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS` / `OMNI_DEV_CLAUDE_CLI_ALLOW_MCP` and the per-call cap `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD`.
2. `USE_OLLAMA=true` → `OpenAiAiClient::new_ollama`.
3. `USE_OPENAI=true` → `OpenAiAiClient::new_openai`.
4. `CLAUDE_CODE_USE_BEDROCK=true` → `BedrockAiClient`.
5. Default → `ClaudeAiClient` (direct Anthropic API).

`src/utils/preflight.rs` mirrors this switch and must change in lock-step when adding backends.

## Data flow

A typical `twiddle` invocation flows through these stages:

```
CLI parsing (clap)
    │
    ▼
Git repository operations (git2)
  ├─ Open repository
  ├─ Resolve commit range
  ├─ Extract CommitInfo for each commit (metadata, diff stats)
  └─ Read working directory status, remotes
    │
    ▼
RepositoryView construction
  ├─ Assemble all commit info, branch info, remotes
  └─ (Optional) Load project context via ProjectDiscovery
       ├─ Commit guidelines from .omni-dev/commit-guidelines.md
       ├─ Scopes from .omni-dev/scopes.yaml + ecosystem defaults
       └─ Branch, file, and work pattern analysis
    │
    ▼
RepositoryView → RepositoryViewForAI
  └─ Expand with full diff content for each commit
    │
    ▼
Prompt construction with token budget fitting
  ├─ Serialize to YAML as user prompt
  ├─ Estimate token count (chars ÷ 3.5 × 1.10 safety margin)
  ├─ If over budget, progressively reduce diff detail:
  │     Full → Truncated → StatOnly → FileListOnly
  └─ Validate final prompt fits model context window
    │
    ▼
AI API request
  ├─ System prompt (from prompts.rs with guidelines + scopes)
  ├─ User prompt (serialized YAML)
  └─ HTTP POST via AiClient implementation
    │
    ▼
Response parsing
  ├─ Parse YAML response (handle markdown-wrapped blocks)
  └─ Deserialize into AmendmentFile or CheckReport
    │
    ▼
(Optional) Coherence pass
  └─ Second AI call to normalize across multiple commits
    │
    ▼
Output / Application
  ├─ twiddle: Apply amendments via git rebase
  ├─ check: Display report with severity-colored output
  └─ create-pr: Create PR via GitHub CLI
```

### Multi-commit processing

When multiple commits are involved, the system uses a map-reduce pattern:

1. **Batching** — Commits are grouped into token-budget-aware batches using first-fit-decreasing bin-packing
2. **Parallel map** — Batches are processed concurrently with semaphore-based concurrency control (default: 4)
3. **Reduce** — An optional coherence pass normalizes results across batches

## Key abstractions

### AiClient trait (`src/claude/ai.rs`)

```rust
pub trait AiClient: Send + Sync {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    fn get_metadata(&self) -> AiClientMetadata;
}
```

Three implementations exist: `ClaudeAiClient` (Anthropic API), `OpenAiAiClient` (OpenAI and Ollama), and `BedrockAiClient` (AWS Bedrock). Provider selection is determined by environment variables at startup in `create_default_claude_client()`.

### ClaudeClient (`src/claude/client.rs`)

The main orchestrator. Wraps a `Box<dyn AiClient>` and provides high-level methods:

- `generate_amendments()` / `generate_contextual_amendments()` — twiddle workflow
- `check_commits_with_scopes()` — check workflow with scope validation
- `generate_pr_content_with_context()` — PR creation workflow
- `refine_amendments_coherence()` / `refine_checks_coherence()` — cross-commit coherence passes

All methods handle token budget fitting internally using progressive diff reduction.

### Model registry (`src/claude/model_config.rs`)

Loads model specifications from an embedded YAML file (`src/templates/models.yaml`). Provides token limits, provider info, and beta header definitions. Supports fuzzy matching for Bedrock-style model identifiers (e.g., `us.anthropic.claude-sonnet-4-5-20250929-v1:0` matches `claude-sonnet-4-5-20250929`).

### Context discovery (`src/claude/context/discovery.rs`)

Resolves project configuration with cascading priority:

```
.omni-dev/local/{file}    ← local override (gitignored)
.omni-dev/{file}           ← project shared config
~/.omni-dev/{file}         ← user home fallback
```

Detects the project ecosystem from marker files (`Cargo.toml` → Rust, `package.json` → Node, etc.) and merges ecosystem-specific default scopes into the project's custom scopes.

### Pre-validation (`src/git/commit.rs`)

Deterministic checks run before AI processing. Scope validity and format are verified locally; passing checks are recorded in `pre_validated_checks` so the AI treats them as authoritative and skips re-checking.

## Extension guide

### Adding a new AI provider

1. Create `src/claude/ai/myprovider.rs` implementing `AiClient`
2. Export from `src/claude/ai.rs`
3. Add provider selection logic in `create_default_claude_client()` (`src/claude/client.rs`) — check an environment variable and construct the implementation
4. Add model entries to `src/templates/models.yaml`

### Adding a new CLI command

1. Create `src/cli/git/mycommand.rs` with a `#[derive(Parser)]` struct and `execute()` method
2. Add a variant to the parent subcommand enum (e.g., `CommitSubcommands` in `src/cli/git.rs`)
3. Wire the execute call in the parent's `execute()` match

### Adding a new output format

1. Add a variant to `OutputFormat` in `src/data/check.rs`
2. Implement the `FromStr` conversion for CLI parsing
3. Add the serialization branch in the command's `output_report()` method

## Dependency rationale

| Crate | Role |
|-------|------|
| `clap` (derive) | CLI parsing with compile-time validation |
| `git2` | Native git operations without shelling out to `git` |
| `reqwest` | HTTP client for AI provider APIs |
| `tokio` | Async runtime for concurrent API requests |
| `serde` + `serde_yaml` | Structured data exchange (YAML is the primary format — see ADR-0001) |
| `serde_json` | JSON parsing for API responses and settings |
| `anyhow` | Application-level error propagation with context chains |
| `thiserror` | Typed errors for the AI client layer (`ClaudeError`) |
| `regex` | Commit message parsing (scope extraction, conventional commit detection) |
| `tracing` | Structured logging controlled via `RUST_LOG` |
| `globset` | File pattern matching for scope refinement |
| `dirs` | Cross-platform home directory resolution for config fallback |
| `crossterm` | Terminal interaction for interactive chat |
| `tempfile` | Temporary files for amendment workflows |
| `chrono` | Date/time handling in commit metadata |
| `url` | URL parsing for remote repository information |
