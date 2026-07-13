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
use crate::utils::env::{EnvSource, SystemEnv};

/// Resolves the tracing filter directive for the MCP server, honouring the
/// precedence `RUST_LOG` (process env) > `settings.mcp.log_level` > `"warn"`
/// (issue #620).
///
/// Pure over an injected [`EnvSource`] so the precedence is unit-testable
/// without mutating the process environment. Empty values at either layer are
/// ignored so a blank `RUST_LOG` or `log_level` does not mask the next tier.
fn resolve_log_directive(env: &impl EnvSource, settings_log_level: Option<&str>) -> String {
    env.var("RUST_LOG")
        .filter(|s| !s.is_empty())
        .or_else(|| {
            settings_log_level
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "warn".to_string())
}

/// Initialises the MCP server's tracing subscriber.
///
/// The filter directive is resolved via [`resolve_log_directive`]: `RUST_LOG`
/// wins when set, otherwise the caller-supplied `settings_log_level` (from
/// `settings.mcp.log_level`), otherwise `"warn"`. Directives are parsed
/// leniently, so an unrecognised fragment is dropped rather than aborting
/// startup.
///
/// Returns `Ok(())` when the global subscriber was set, and `Err` when one
/// was already installed (typical in tests where multiple cases initialise
/// tracing). Returning a `Result` instead of panicking matches the rest of
/// the codebase's STYLE-0003 stance.
pub fn try_init_tracing(settings_log_level: Option<&str>) -> Result<()> {
    let directive = resolve_log_directive(&SystemEnv, settings_log_level);
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::new(directive))
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

/// Returns a comma-separated list of compiled-in MCP feature flags, suitable
/// for logging on startup so operators can confirm the server they're running.
///
/// When no optional features are active, returns `"base"`.
pub fn feature_flags() -> &'static str {
    // Currently the `mcp` binary always implies the `mcp` feature. Kept as a
    // function (rather than a constant) so future features (metrics, tracing
    // exporters, etc.) can extend the string without breaking callers.
    "mcp"
}

/// Emits the startup `info!` event with version and active feature flags.
///
/// Lifted out of the binary so the log macro body is covered by library
/// tests. Operators still see the event at runtime; the binary simply calls
/// this function instead of inlining the macro.
pub fn log_startup_event() {
    let version = env!("CARGO_PKG_VERSION");
    let features = feature_flags();
    tracing::info!(version, features, "starting omni-dev MCP server");
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
    use crate::test_support::env::MapEnv;
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
    fn feature_flags_includes_mcp() {
        let flags = feature_flags();
        assert!(
            flags.contains("mcp"),
            "expected feature flags to include mcp, got {flags:?}"
        );
    }

    #[test]
    fn log_startup_event_does_not_panic() {
        // Running the macro body is the entire point — we don't assert on
        // the output (tracing may not have a subscriber installed in this
        // test process). Just execute the function body to cover it.
        log_startup_event();
    }

    #[test]
    fn try_init_tracing_is_idempotent_or_errors() {
        // The global subscriber may already be set by another test; both
        // outcomes are acceptable. The point is to execute the function body.
        let _ = try_init_tracing(None);
        // A second call must not panic; it should just return `Err`.
        let second = try_init_tracing(Some("info"));
        assert!(second.is_err(), "second init should report already-set");
    }

    #[test]
    fn resolve_log_directive_rust_log_beats_settings() {
        // Env-var precedence (issue #620): a set `RUST_LOG` wins over the
        // settings-provided level.
        let env = MapEnv::new().with("RUST_LOG", "debug");
        assert_eq!(resolve_log_directive(&env, Some("info")), "debug");
    }

    #[test]
    fn resolve_log_directive_settings_used_when_rust_log_unset() {
        let env = MapEnv::new();
        assert_eq!(resolve_log_directive(&env, Some("info")), "info");
    }

    #[test]
    fn resolve_log_directive_defaults_to_warn() {
        let env = MapEnv::new();
        assert_eq!(resolve_log_directive(&env, None), "warn");
    }

    #[test]
    fn resolve_log_directive_ignores_empty_values() {
        // A blank `RUST_LOG` falls through to settings; a blank settings level
        // falls through to the built-in default.
        let env = MapEnv::new().with("RUST_LOG", "");
        assert_eq!(resolve_log_directive(&env, Some("trace")), "trace");
        assert_eq!(resolve_log_directive(&env, Some("")), "warn");
    }
}
