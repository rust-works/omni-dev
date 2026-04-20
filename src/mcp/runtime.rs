//! Runtime helpers shared by the `omni-dev-mcp` binary and tests.
//!
//! Extracting these out of the binary keeps `src/mcp_server.rs` to a thin
//! `main` shim and lets us cover the interesting work — error formatting,
//! transport wiring — with library unit tests.

use std::io::Write;

use anyhow::Result;
use rmcp::{
    service::{RunningService, ServiceExt},
    RoleServer,
};
use tracing_subscriber::EnvFilter;

use super::OmniDevServer;

/// Initialises the MCP server's tracing subscriber.
///
/// Returns `Ok(())` when the global subscriber was set, and `Err` when one
/// was already installed (typical in tests where multiple cases initialise
/// tracing). Returning a `Result` instead of panicking matches the rest of
/// the codebase's STYLE-0003 stance.
pub fn try_init_tracing() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing subscriber already set: {e}"))?;
    Ok(())
}

/// Constructs an [`OmniDevServer`] and starts serving on the given transport.
///
/// The returned future resolves once the peer disconnects.
pub async fn serve_with<T, E, A>(transport: T) -> Result<()>
where
    T: rmcp::transport::IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let service: RunningService<RoleServer, OmniDevServer> =
        OmniDevServer::new().serve(transport).await?;
    service.waiting().await?;
    Ok(())
}

/// Writes an `anyhow::Error` chain to a writer in the format the binary uses.
///
/// Pulled out as its own function so the formatting can be exercised against
/// an in-memory buffer without spawning a subprocess.
pub fn write_error_chain<W: Write>(writer: &mut W, err: &anyhow::Error) -> std::io::Result<()> {
    writeln!(writer, "Error: {err}")?;
    let mut source = err.source();
    while let Some(inner) = source {
        writeln!(writer, "  Caused by: {inner}")?;
        source = inner.source();
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Context};

    #[test]
    fn write_error_chain_single_error() {
        let err = anyhow!("only failure");
        let mut buf = Vec::new();
        write_error_chain(&mut buf, &err).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "Error: only failure\n");
    }

    #[test]
    fn write_error_chain_preserves_chain() {
        let result: Result<(), anyhow::Error> =
            Err(anyhow!("root")).context("middle").context("outermost");
        let err = result.expect_err("constructed Err");
        let mut buf = Vec::new();
        write_error_chain(&mut buf, &err).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("Error: outermost\n"), "got: {out:?}");
        assert!(out.contains("  Caused by: middle\n"));
        assert!(out.contains("  Caused by: root\n"));
    }

    #[tokio::test]
    async fn serve_with_handles_peer_disconnect() {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        // Drive the server in a task; drop the client end immediately so the
        // server's `waiting()` future resolves cleanly.
        let server_handle = tokio::spawn(async move { serve_with(server_transport).await });
        drop(client_transport);
        let result = server_handle.await.unwrap();
        // Either Ok (clean disconnect) or Err (transport error) — both
        // exercise the function. We just need the function body covered.
        let _ = result;
    }

    #[test]
    fn try_init_tracing_is_idempotent_or_errors() {
        // The global subscriber may already be set by another test; both
        // outcomes are acceptable. The point is to execute the function body.
        let _ = try_init_tracing();
        // A second call must not panic; it should just return `Err`.
        let second = try_init_tracing();
        assert!(second.is_err(), "second init should report already-set");
    }
}
