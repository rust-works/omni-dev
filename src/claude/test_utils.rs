//! Shared test utilities for the `claude` module.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::claude::ai::{AiClient, AiClientMetadata};

/// Mock AI client with a pre-programmed queue of responses.
///
/// Responses are returned in FIFO order. When the queue is exhausted,
/// subsequent calls return `Err("no more mock responses")`.
///
/// # Example
///
/// ```rust
/// let client = ClaudeClient::new(Box::new(ConfigurableMockAiClient::new(vec![
///     Err(anyhow::anyhow!("rate limit")),  // batch attempt fails
///     Ok("amendments:\n  - commit: ...".to_string()),  // retry succeeds
/// ])));
/// ```
pub(crate) struct ConfigurableMockAiClient {
    responses: Arc<Mutex<VecDeque<Result<String>>>>,
    metadata: AiClientMetadata,
}

impl ConfigurableMockAiClient {
    /// Creates a new mock client that will return the given responses in order.
    pub(crate) fn new(responses: Vec<Result<String>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            metadata: AiClientMetadata {
                provider: "Mock".to_string(),
                model: "mock-model".to_string(),
                max_context_length: 200_000,
                max_response_length: 8_192,
                active_beta: None,
            },
        }
    }

    /// Returns a new mock client with a custom context window size.
    ///
    /// Useful for testing split-dispatch behaviour with a small budget.
    pub(crate) fn with_context_length(mut self, max_context_length: usize) -> Self {
        self.metadata.max_context_length = max_context_length;
        self
    }

    /// Returns a handle that can be used to inspect the response queue
    /// after the mock client has been moved into a [`ClaudeClient`].
    pub(crate) fn response_handle(&self) -> ResponseQueueHandle {
        ResponseQueueHandle {
            responses: self.responses.clone(),
        }
    }
}

/// Shared handle to a mock client's response queue.
///
/// Holds an `Arc` reference to the same queue used by the mock client,
/// allowing tests to inspect how many responses remain after execution.
pub(crate) struct ResponseQueueHandle {
    responses: Arc<Mutex<VecDeque<Result<String>>>>,
}

impl ResponseQueueHandle {
    /// Returns the number of unconsumed responses remaining in the queue.
    pub(crate) fn remaining(&self) -> usize {
        self.responses.lock().unwrap().len()
    }
}

impl AiClient for ConfigurableMockAiClient {
    fn send_request<'a>(
        &'a self,
        _system_prompt: &'a str,
        _user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        let responses = self.responses.clone();
        Box::pin(async move {
            responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no more mock responses")))
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        self.metadata.clone()
    }
}
