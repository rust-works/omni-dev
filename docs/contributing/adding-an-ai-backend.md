# Adding an AI backend

omni-dev talks to AI providers through the `AiClient` trait and a runtime
factory that selects an implementation based on environment variables. The
overall design is recorded in [ADR-0002](../adrs/adr-0002.md). This recipe
walks you through adding a hypothetical new backend (say, "Mistral"),
mirroring the existing
[`src/claude/ai/openai.rs`](../../src/claude/ai/openai.rs) for an HTTP-based
provider.

## Files you'll touch

| File | Edit |
|---|---|
| [`src/claude/ai/mistral.rs`](../../src/claude/ai/) (new) | The new backend module. |
| [`src/claude/ai.rs`](../../src/claude/ai.rs) | Add `pub mod mistral;` and re-export if needed. |
| [`src/claude/client.rs`](../../src/claude/client.rs) | Add a dispatch branch to `create_default_claude_client`. |
| [`src/utils/preflight.rs`](../../src/utils/preflight.rs) | Add a matching branch to `check_ai_credentials` and a variant to `AiProvider`. |
| [`src/templates/models.yaml`](../../src/templates/models.yaml) | Register the provider's models in the registry. |
| [`docs/adrs/adr-0002.md`](../adrs/adr-0002.md) | Add a row to the backend table. |

## The trait

```rust
pub trait AiClient: Send + Sync {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    fn get_metadata(&self) -> AiClientMetadata;

    fn capabilities(&self) -> AiClientCapabilities { /* default: all disabled */ }

    fn send_request_with_options<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
        _options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> { /* default delegates */ }
}
```

Defined in [`src/claude/ai.rs:222`](../../src/claude/ai.rs#L222). The trait
lives in `ai.rs`, **not** `client.rs` — only the factory `ClaudeClient`
wrapper lives in `client.rs`.

## Walkthrough

### 1. The backend module

Create [`src/claude/ai/mistral.rs`](../../src/claude/ai/) following the
shape of [`openai.rs`](../../src/claude/ai/openai.rs):

```rust
use std::pin::Pin;
use std::future::Future;
use anyhow::Result;
use reqwest::Client;

use super::{
    build_http_client, check_error_response, log_response_success,
    registry_model_limits, AiClient, AiClientMetadata,
};

pub struct MistralAiClient {
    client: Client,
    model: String,
    api_key: String,
    base_url: String,
    active_beta: Option<(String, String)>,
}

impl MistralAiClient {
    pub fn new(
        model: String,
        api_key: String,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        Ok(Self {
            client: build_http_client()?,
            model,
            api_key,
            base_url: "https://api.mistral.ai/v1".to_string(),
            active_beta,
        })
    }
}

impl AiClient for MistralAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            // POST {base_url}/chat/completions
            // Parse response, call check_error_response on non-2xx,
            // call log_response_success on success.
            todo!()
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        let limits = registry_model_limits(&self.model, "mistral");
        AiClientMetadata {
            provider: "mistral".to_string(),
            model: self.model.clone(),
            max_context_length: limits.input_context,
            max_response_length: limits.max_output_tokens,
            active_beta: self.active_beta.clone(),
        }
    }
}
```

**Reuse the shared helpers** in [`src/claude/ai.rs`](../../src/claude/ai.rs):

- `build_http_client()` — applies the project-wide 5-minute timeout.
- `registry_model_limits(model, provider)` / `registry_max_output_tokens(...)` — looks up token limits from `models.yaml`.
- `check_error_response(...)` — flattens non-2xx HTTP responses into a useful `anyhow::Error`.
- `log_response_success(...)` — emits the standard `tracing` event.

Don't roll your own — that's how you avoid the rest of the codebase
noticing your backend exists.

### 2. Extend the shared resolver, then wire the factory

Backend/model/beta-header selection lives in one place:
[`src/claude/backend.rs`](../../src/claude/backend.rs). Add:

1. A variant to the `AiBackend` enum (with an explicit `#[value(name =
   "mistral")]` so the `--ai-backend` value renders correctly) and arms in
   `env_value` / `from_env_value`.
2. A model-resolution arm in `resolve_model` (which family vars / registry
   default apply). Add named constants for any new env vars.

`--ai-backend mistral` and `OMNI_DEV_AI_BACKEND=mistral` then work with no
further wiring. Only add a legacy-style `USE_MISTRAL=true` selector to
`resolve_backend` if you have a compatibility reason — new backends should
be selected via `OMNI_DEV_AI_BACKEND` alone.

Then add one dispatch arm to the `match` in
`create_default_claude_client_with`
([`src/claude/client.rs`](../../src/claude/client.rs)): call
`validate_beta_header`, look up the API key, construct the client, return
`Ok(ClaudeClient::new(Box::new(ai_client)))`. The existing arms show the
pattern.

### 3. Add the preflight arm — **same PR**

[`src/utils/preflight.rs`](../../src/utils/preflight.rs) — the
`check_ai_credentials_with` function matches on the same resolved
`AiBackend` and runs **before** any backend is constructed. Because it
shares `resolve_backend`/`resolve_model`, selection and model resolution
cannot drift; you only supply the credential check.

Add a variant to the `AiProvider` enum at
[`src/utils/preflight.rs:22`](../../src/utils/preflight.rs#L22), then add a
match arm that verifies the API key (same lookup as the factory arm) and
returns `Ok(AiCredentialInfo { provider: AiProvider::Mistral, model })`.

### 4. Register models

Add rows to [`src/templates/models.yaml`](../../src/templates/models.yaml)
for each supported Mistral model, with the same fields the Claude/OpenAI
rows use (`provider`, `model`, `api_identifier`, `max_output_tokens`,
`input_context`, `tier`). The model registry — see
[ADR-0022](../adrs/adr-0022.md) — picks these up automatically and the
`omni-dev config models show` command surfaces them to users.

### 5. Update ADR-0002

[ADR-0002](../adrs/adr-0002.md) has a table of supported backends. Add a
row for the new HTTP backend. A **sandboxed-subprocess** backend (like
`claude_cli`) is structurally different enough to warrant its own ADR —
file one and link to ADR-0002 from it.

## Testing

Inline backend tests follow two patterns:

**Mock the trait, not the network.** For tests of code that *consumes* the
trait (e.g. amendment parsing), use the inline mocks at
[`src/claude/client.rs:1777-1880`](../../src/claude/client.rs#L1777-L1880):

- `MockAiClient` — returns an empty string.
- `SchemaRecordingMockAiClient` — records the `RequestOptions` it receives
  so you can assert structured-output schemas are forwarded correctly.

**Dispatch tests.** [`src/claude/client.rs:3965+`](../../src/claude/client.rs#L3965)
uses an `EnvGuard` helper to manipulate `OMNI_DEV_AI_BACKEND`, `USE_*`,
etc., then calls `create_default_claude_client(...).await` and asserts on
`get_metadata().provider`. Add at least one test per new backend that
proves it's selectable.

**Preflight tests.** [`src/utils/preflight.rs`](../../src/utils/preflight.rs)
(around lines 336–556) uses the same `EnvGuard` pattern and, for the
claude-cli backend, a `make_version_shim()` helper that creates a fake
`claude --version` script. Reuse `EnvGuard`; reach for `make_version_shim`
only if your backend also probes an external binary.

No `wiremock`-based tests for the HTTP backends currently exist — they're
exercised end-to-end in CI against real APIs (or skipped). Adding wiremock
coverage is welcome but not required.

## Gotchas

- **Factory and preflight share the resolver but not the credential
  checks.** Selection/model drift is impossible (both call
  `crate::claude::backend`), but a missing preflight match arm won't
  compile only because `AiBackend` is exhaustively matched — keep the
  credential logic in both arms consistent by hand.
- **Beta headers are Anthropic-specific.** The factory calls
  `validate_beta_header(&model, &beta_header)?` for HTTP backends. Models
  with no beta-header table just get a no-op validation. For backends that
  ignore beta headers entirely (e.g. `claude_cli`), log a warning and drop
  it — see [`src/claude/client.rs:1636-1641`](../../src/claude/client.rs#L1636-L1641).
- **Provider-specific prompt shaping** is documented in
  [ADR-0014](../adrs/adr-0014.md). Don't reshape prompts inside the
  backend module unless the API genuinely needs it (e.g. OpenAI's
  `messages[0]` system role vs. Anthropic's top-level `system` field).
- **Selection is the global `--ai-backend` flag / `OMNI_DEV_AI_BACKEND`.**
  New enum variants surface in the flag's possible values automatically —
  which also means the help snapshot changes, so run the `update-snapshots`
  skill. Don't invent per-backend `USE_*` selector vars; those are legacy.

## ADRs

- [ADR-0002](../adrs/adr-0002.md) — Multi-Provider AI Abstraction via Trait Objects (primary; add a row).
- [ADR-0007](../adrs/adr-0007.md) — Preflight Validation Pattern (governs the preflight integration).
- [ADR-0014](../adrs/adr-0014.md) — Provider-Specific Prompt Engineering.
- [ADR-0022](../adrs/adr-0022.md) — Layered Model Catalog with User and Project Overrides.
