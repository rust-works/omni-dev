# AI Client Architecture Plan

## Overview

This document outlines the plan for refactoring the Claude API client into a more abstract and flexible architecture using the `AiClient` trait. This will allow for multiple AI service implementations while maintaining a consistent interface.

## Architecture

```
┌─────────────────┐     ┌──────────────────────┐
│                 │     │                      │
│   ClaudeClient  │────▶│      AiClient        │
│                 │     │       (trait)        │
└─────────────────┘     └──────────────────────┘
                               ▲
                               │
                               │
                        ┌──────┴───────┐
                        │              │
                  ┌─────┴────┐   ┌─────┴────┐
                  │          │   │          │
                  │ Claude   │   │  Future  │
                  │ AiClient │   │ AiClient │
                  │          │   │          │
                  └──────────┘   └──────────┘
```

## Core Components

### AiClient Trait

```rust
/// Trait for AI service clients
pub trait AiClient: Send + Sync {
    /// Send a request to the AI service and return the raw response
    async fn send_request(&self, system_prompt: &str, user_prompt: &str) -> Result<String>;

    /// Get metadata about the AI client implementation
    fn get_metadata(&self) -> AiClientMetadata;
}

/// Metadata about an AI client implementation
pub struct AiClientMetadata {
    /// Service provider name
    pub provider: String,
    /// Model identifier
    pub model: String,
    /// Maximum context length supported
    pub max_context_length: usize,
    /// Maximum token response length supported
    pub max_response_length: usize,
}
```

### ClaudeAiClient Implementation

```rust
/// Claude API client implementation
pub struct ClaudeAiClient {
    /// HTTP client for API requests
    client: reqwest::Client,
    /// API key for authentication
    api_key: String,
    /// Model identifier
    model: String,
}

impl ClaudeAiClient {
    /// Create a new Claude AI client
    pub fn new(model: String, api_key: Option<String>) -> Result<Self> {
        // Get API key from provided value or environment
        let api_key = match api_key {
            Some(key) => key,
            None => std::env::var("CLAUDE_API_KEY")
                .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
                .map_err(|_| ClaudeError::ApiKeyNotFound)?,
        };

        let client = reqwest::Client::new();

        Ok(Self {
            client,
            api_key,
            model,
        })
    }
}

impl AiClient for ClaudeAiClient {
    async fn send_request(&self, system_prompt: &str, user_prompt: &str) -> Result<String> {
        // Build request to Claude API
        let request = ClaudeRequest {
            model: self.model.clone(),
            max_tokens: 4000,  // Consider making this configurable
            system: system_prompt.to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            }],
        };

        // Send request
        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

        // Process response
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(ClaudeError::ApiRequestFailed(format!("HTTP {}: {}", status, error_text)).into());
        }

        let claude_response: ClaudeResponse = response
            .json()
            .await
            .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

        // Extract text content
        claude_response
            .content
            .first()
            .filter(|c| c.content_type == "text")
            .map(|c| c.text.clone())
            .ok_or_else(|| {
                ClaudeError::InvalidResponseFormat("No text content in response".to_string()).into()
            })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        AiClientMetadata {
            provider: "Anthropic".to_string(),
            model: self.model.clone(),
            max_context_length: 100000,  // Adjust based on model
            max_response_length: 4000,   // Default response length
        }
    }
}
```

### Refactored ClaudeClient

```rust
/// Claude client for commit message improvement
pub struct ClaudeClient {
    /// AI client implementation
    ai_client: Box<dyn AiClient>,
}

impl ClaudeClient {
    /// Create new Claude client with provided AI client implementation
    pub fn new(ai_client: Box<dyn AiClient>) -> Self {
        Self { ai_client }
    }

    /// Generate commit message amendments from repository view
    pub async fn generate_amendments(&self, repo_view: &RepositoryView) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate user prompt
        let user_prompt = prompts::generate_user_prompt(&repo_yaml);

        // Send request using AI client
        let content = self.ai_client
            .send_request(prompts::SYSTEM_PROMPT, &user_prompt)
            .await?;

        // Parse YAML response
        self.parse_amendment_response(&content)
    }

    /// Generate contextual commit message amendments
    pub async fn generate_contextual_amendments(
        &self,
        repo_view: &RepositoryView,
        context: &crate::data::context::CommitContext,
    ) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate contextual prompts
        let system_prompt = prompts::generate_contextual_system_prompt(context);
        let user_prompt = prompts::generate_contextual_user_prompt(&repo_yaml, context);

        // Send request using AI client
        let content = self.ai_client
            .send_request(&system_prompt, &user_prompt)
            .await?;

        // Parse YAML response
        self.parse_amendment_response(&content)
    }

    /// Parse Claude's YAML response into AmendmentFile
    fn parse_amendment_response(&self, content: &str) -> Result<AmendmentFile> {
        // [Existing parsing code remains unchanged]
    }
}
```

## Factory Function for Convenience

```rust
/// Create a default Claude client using environment variables
pub fn create_default_claude_client(model: Option<String>) -> Result<ClaudeClient> {
    let model = model.unwrap_or_else(|| "claude-opus-4-1-20250805".to_string());
    let ai_client = ClaudeAiClient::new(model, None)?;
    Ok(ClaudeClient::new(Box::new(ai_client)))
}
```

## Integration Plan

1. Create the `AiClient` trait and `AiClientMetadata` struct
2. Implement `ClaudeAiClient` that satisfies the trait
3. Refactor `ClaudeClient` to use the `AiClient` trait
4. Update all places that construct `ClaudeClient` to use the new pattern
5. Add a factory function for backward compatibility where needed
6. Add tests for the new abstractions

## Testing Strategy

1. Unit tests for each implementation of `AiClient`
2. Integration tests that verify the `ClaudeClient` works correctly with different `AiClient` implementations
3. Mock implementation of `AiClient` for testing purposes

## Future Expansion

This architecture allows for easy addition of new AI providers:

1. Create a new struct (e.g., `BedrockAiClient`)
2. Implement the `AiClient` trait for it
3. The existing `ClaudeClient` can use this new implementation without changes

## Error Handling

The `AiClient` trait will return general errors that can be converted to specific application errors as needed. Consider adding an `AiClientError` enum for standardized error handling across implementations.

## API Compatibility Considerations

Some AI providers may require different parameter structures. The `AiClient` trait should be designed to handle common denominator functionality while allowing specific implementations to optimize for their APIs.