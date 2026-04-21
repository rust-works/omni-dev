//! Cancellation helpers for MCP tool handlers.
//!
//! `rmcp` threads a `CancellationToken` through every tool invocation via
//! `RequestContext`. When a client disconnects or explicitly cancels a
//! request, the token fires and in-flight tool handlers should return
//! promptly.
//!
//! For async tools this is straightforward — pass the token through to
//! underlying clients that support cancellation. For blocking work (libgit2
//! calls) we wrap the `spawn_blocking` future in a `select!` with the token.
//! The OS thread itself can't be cancelled mid-flight, but the handler
//! returns early and the client sees a clean cancellation error instead of
//! waiting for the blocking call to complete.

use std::future::Future;

use anyhow::Result;
use rmcp::ErrorData as McpError;
use tokio_util::sync::CancellationToken;

/// Error message surfaced when a request is cancelled.
const CANCELLED_MESSAGE: &str = "request cancelled";

/// Runs a blocking closure on the `tokio` blocking thread pool, returning
/// early if the cancellation token fires.
///
/// Cancellation is cooperative at the OS thread level — the blocking closure
/// continues to run to completion on the blocking pool, but its result is
/// discarded. The future returned here resolves as soon as the token fires.
pub async fn spawn_blocking_cancellable<F, T>(
    token: &CancellationToken,
    f: F,
) -> Result<T, McpError>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::task::spawn_blocking(f);
    tokio::select! {
        () = token.cancelled() => Err(cancelled_error()),
        join = handle => match join {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(domain)) => Err(crate::mcp::error::tool_error(domain)),
            Err(join) => Err(crate::mcp::error::tool_error(
                anyhow::anyhow!("join error: {join}"),
            )),
        },
    }
}

/// Awaits a future, returning early if the cancellation token fires.
///
/// Use for async tool work (Atlassian, AI) where the underlying client
/// doesn't accept a cancellation token directly.
pub async fn cancellable<F, T>(token: &CancellationToken, fut: F) -> Result<T, McpError>
where
    F: Future<Output = Result<T, McpError>>,
{
    tokio::select! {
        () = token.cancelled() => Err(cancelled_error()),
        result = fut => result,
    }
}

/// Builds the `McpError` returned when a request is cancelled.
///
/// Exposed for tests and for direct use in cancellation branches of
/// `select!` invocations elsewhere.
pub fn cancelled_error() -> McpError {
    McpError::internal_error(CANCELLED_MESSAGE.to_string(), None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_blocking_returns_value_when_not_cancelled() {
        let token = CancellationToken::new();
        let value = spawn_blocking_cancellable(&token, || Ok::<_, anyhow::Error>(42))
            .await
            .unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn spawn_blocking_returns_early_on_cancellation() {
        let token = CancellationToken::new();
        let child = token.child_token();
        // Cancel from a separate task shortly after the blocking future starts.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            token.cancel();
        });
        let result: Result<(), _> = spawn_blocking_cancellable(&child, || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(())
        })
        .await;
        let err = result.expect_err("should be cancelled");
        assert!(err.message.contains("cancelled"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn spawn_blocking_surfaces_join_error_when_task_panics() {
        let token = CancellationToken::new();
        let result: Result<(), _> = spawn_blocking_cancellable(&token, || {
            panic!("blocking task panicked");
        })
        .await;
        let err = result.expect_err("panic should surface as a join error");
        assert!(
            err.message.contains("join error"),
            "expected join error, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn spawn_blocking_propagates_domain_error() {
        let token = CancellationToken::new();
        let result: Result<(), _> =
            spawn_blocking_cancellable(&token, || Err(anyhow::anyhow!("domain failure"))).await;
        let err = result.expect_err("should propagate error");
        assert!(err.message.contains("domain failure"));
    }

    #[tokio::test]
    async fn cancellable_returns_value_when_not_cancelled() {
        let token = CancellationToken::new();
        let value = cancellable(&token, async { Ok::<_, McpError>(7) })
            .await
            .unwrap();
        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn cancellable_returns_early_on_cancellation() {
        let token = CancellationToken::new();
        let child = token.child_token();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            token.cancel();
        });
        // `std::future::pending()` never resolves, so `cancellable` must
        // return via the cancellation branch of the `select!`. Using
        // `pending()` (rather than a sleep-then-`Ok(())` async block) avoids
        // leaving uncovered body lines in the test source.
        let result: Result<(), _> = cancellable(&child, std::future::pending()).await;
        let err = result.expect_err("should be cancelled");
        assert!(err.message.contains("cancelled"));
    }

    #[tokio::test]
    async fn cancellable_preserves_inner_error() {
        let token = CancellationToken::new();
        let inner = McpError::invalid_params("bad".to_string(), None);
        let result: Result<(), _> =
            cancellable(&token, async move { Err::<(), _>(inner.clone()) }).await;
        let err = result.expect_err("should propagate inner error");
        assert!(err.message.contains("bad"));
    }

    #[test]
    fn cancelled_error_has_expected_message() {
        let err = cancelled_error();
        assert_eq!(err.message, CANCELLED_MESSAGE);
    }
}
