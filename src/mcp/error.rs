//! Error mapping between `anyhow::Error` chains and MCP tool errors.

use rmcp::ErrorData as McpError;

/// Converts an `anyhow::Error` into an `McpError` suitable for returning from
/// a tool handler.
///
/// The entire error chain is flattened into a single human-readable string so
/// the client sees full diagnostic context in one payload. We emit an
/// `internal_error` code because most domain failures we surface (git I/O,
/// API failures, configuration errors) are opaque to the caller — they can
/// read the message but not programmatically recover.
pub fn tool_error(err: anyhow::Error) -> McpError {
    let mut message = format!("{err}");
    let mut source = err.source();
    while let Some(inner) = source {
        message.push_str(&format!("\n  Caused by: {inner}"));
        source = inner.source();
    }
    McpError::internal_error(message, None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Context};

    #[test]
    fn single_error_flattens_to_message() {
        let err = anyhow!("top-level failure");
        let mcp = tool_error(err);
        assert!(
            mcp.message.contains("top-level failure"),
            "message was: {}",
            mcp.message
        );
    }

    #[test]
    fn error_chain_preserved_in_message() {
        let result: Result<(), anyhow::Error> = Err(anyhow!("root cause"))
            .context("middle context")
            .context("outermost");
        let err = result.expect_err("constructed Err");
        let mcp = tool_error(err);
        assert!(mcp.message.contains("outermost"));
        assert!(mcp.message.contains("Caused by: middle context"));
        assert!(mcp.message.contains("Caused by: root cause"));
    }
}
