# Adding an MCP tool

omni-dev's MCP server exposes operations to LLM clients via `rmcp`-generated
tool routers. The server architecture is recorded in
[ADR-0021](../adrs/adr-0021.md). This recipe walks you through adding one new
tool end-to-end, mirroring the existing `git_*` tools in
[`src/mcp/git_tools.rs`](../../src/mcp/git_tools.rs).

## Files you'll touch

| File | Edit |
|---|---|
| [`src/mcp/<area>_tools.rs`](../../src/mcp/) (new or existing) | Add a parameter struct and a tool handler method. |
| [`src/mcp/server.rs`](../../src/mcp/server.rs) | Wire the new router into `OmniDevServer::new()` (only if the module is new). |
| Tests inline or in [`tests/mcp_integration_test.rs`](../../tests/mcp_integration_test.rs) | Exercise the handler. |

## Walkthrough

Suppose we want a `git_log_oneline` tool that returns the recent commit log
as YAML. The pattern below matches every existing handler in
[`git_tools.rs`](../../src/mcp/git_tools.rs).

### 1. Parameter struct

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitLogOnelineParams {
    /// Maximum number of commits to return. Defaults to 20.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}
```

Conventions enforced across the codebase:

- Derive `Debug, Deserialize, schemars::JsonSchema`. Doc comments on each
  field become the JSON-schema descriptions the MCP client sees.
- Use `#[serde(default)]` + `Option<T>` for optional fields. Don't invent
  custom validation logic; centralised helpers like `validate_range` and
  `validate_repo_path` live in [`src/mcp/validate.rs`](../../src/mcp/validate.rs).

### 2. Handler

Add the method to the `#[tool_router]` impl block — the one starting at
[`src/mcp/git_tools.rs:103`](../../src/mcp/git_tools.rs#L103):

```rust
/// Tool: return the recent commit log as YAML.
#[tool(
    description = "Return the recent commit log (oneline-style) as YAML. \
                   Mirrors `git log --oneline`."
)]
pub async fn git_log_oneline(
    &self,
    Parameters(params): Parameters<GitLogOnelineParams>,
) -> Result<CallToolResult, McpError> {
    let limit = params.limit.unwrap_or(20);
    let repo_path = params.repo_path.clone();

    let yaml = tokio::task::spawn_blocking(move || {
        crate::cli::git::run_log_oneline(limit, repo_path.as_deref())
    })
    .await
    .map_err(|e| tool_error(anyhow::anyhow!("join error: {e}")))?
    .map_err(tool_error)?;

    Ok(CallToolResult::success(vec![Content::text(yaml)]))
}
```

Conventions:

- **Signature**: `async fn name(&self, Parameters(params): Parameters<T>) -> Result<CallToolResult, McpError>`.
  The `Parameters<T>` wrapper comes from `rmcp::handler::server::wrapper`.
- **Return**: `Ok(CallToolResult::success(vec![Content::text(yaml_string)]))`.
- **Errors**: wrap any `anyhow::Error` via `tool_error(...)` from
  [`src/mcp/error.rs`](../../src/mcp/error.rs) — it walks the `source` chain
  and flattens to a single message so MCP clients see full diagnostics.
- **Blocking work**: wrap in `tokio::task::spawn_blocking`. Long-running
  tools that should be cancellable use
  `spawn_blocking_cancellable(&cancellation, ...)` and accept a
  `CancellationToken` parameter — see `git_view_commits` at
  [`src/mcp/git_tools.rs:110`](../../src/mcp/git_tools.rs#L110).
- **Large responses**: use `build_truncated_result(yaml)` (see
  [`src/mcp/git_tools.rs:243`](../../src/mcp/git_tools.rs#L243)). It emits a
  second `Content::text` JSON payload with `{"truncated": true, ...}` when
  the response exceeds `DEFAULT_MAX_RESPONSE_BYTES`.
- **Mirror the CLI**: tool handlers should delegate to the same `run_*`
  function the CLI uses (e.g. `crate::cli::git::run_view`). Don't duplicate
  logic — MCP is a transport, not a fork.

### 3. Register the router (only for a new module)

If the new tool lives in an existing `*_tools.rs` module, nothing further is
needed — the `#[tool_router(router = X, vis = "pub")]` macro re-discovers all
`#[tool]`-annotated methods in that impl block.

If you've added a new module (say, `src/mcp/foo_tools.rs` with
`#[tool_router(router = foo_tool_router, vis = "pub")]`), wire it into the
combined router in
[`src/mcp/server.rs:43-50`](../../src/mcp/server.rs#L43-L50):

```rust
tool_router: Self::git_tool_router()
    + Self::jira_tool_router()
    + ...
    + Self::foo_tool_router(),
```

The `#[tool_handler(router = self.tool_router)]` on
[`src/mcp/server.rs:56`](../../src/mcp/server.rs#L56) dispatches calls to
the combined router automatically.

## Exposing a resource (optional)

Tools take parameters and return content; **resources** are URI-addressable
content an MCP client fetches without a tool call (e.g.
`omni-dev://specs/jfm`, `jira://issue/PROJ-1`). If your extension is
better modelled as a resource than a tool:

1. Add a variant to `ResourceUri` and a parse arm in `ResourceUri::parse()`
   in [`src/mcp/resources.rs`](../../src/mcp/resources.rs).
2. Add a dispatch arm in `read_resource()` in the same file.
3. Add a `ResourceTemplate` row to `list_resource_templates_result()`.
4. For embedded reference docs (like `omni-dev://specs/<name>`), add a
   `pub const SPEC_X: &str = include_str!("...")` and a `lookup` arm in
   [`src/mcp/specs.rs`](../../src/mcp/specs.rs). Specs are embedded at
   compile time so installed builds don't read from disk.

## Testing

The codebase uses two complementary patterns — pick whichever fits.

**Direct-handler unit tests** are the lightest: construct an `OmniDevServer`
and call the handler. See
[`src/mcp/git_tools.rs:292-470`](../../src/mcp/git_tools.rs#L292-L470):

```rust
#[tokio::test]
async fn git_branch_info_returns_yaml() {
    let server = OmniDevServer::new();
    let result = server
        .git_branch_info(Parameters(GitBranchInfoParams {
            branch: None,
            repo_path: Some(test_repo_path()),
        }))
        .await
        .unwrap();
    // assert on result.content
}
```

**End-to-end integration tests** spin up the server over an in-memory
transport and call it through a real `rmcp` client — see
[`tests/mcp_integration_test.rs`](../../tests/mcp_integration_test.rs)
(the `TestRepo` fixture and `tokio::io::duplex()` transport).

**Tools that hit external APIs** (JIRA, Confluence, Datadog) test the
underlying YAML-helper layer against a `wiremock::MockServer`, not the MCP
handler itself — see top of
[`src/mcp/jira_tools.rs`](../../src/mcp/jira_tools.rs). This avoids needing
credentials in CI.

## Gotchas

- The `#[tool_router]` macro requires both `router = <name>` **and**
  `vis = "pub"` — without `vis`, the generated fn is private and won't
  compose with `+` in `server.rs`.
- The `#[allow(missing_docs)]` above each `#[tool_router]` impl block is
  load-bearing — the macro generates a public fn without a doc comment.
- Don't add a `serde(deny_unknown_fields)` to parameter structs — MCP
  clients may pass forward-compat fields and you want to ignore them.
- The MCP boundary is non-interactive: tools must never prompt or open an
  editor. `git_twiddle_commits` documents the `--auto-apply` workaround in
  its `dry_run` field doc — copy that pattern if your tool wraps a CLI
  command that's normally interactive.

## ADRs

- [ADR-0021](../adrs/adr-0021.md) — MCP Server via Second Binary with `rmcp`.
