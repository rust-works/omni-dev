//! Binary entry point for the omni-dev MCP server.
//!
//! Speaks the Model Context Protocol over stdio so AI assistants can
//! invoke omni-dev's business logic as MCP tools. See [ADR-0021].

use std::process;

use anyhow::Result;
use omni_dev::mcp::OmniDevServer;
use rmcp::{transport::stdio, ServiceExt};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        let mut source = e.source();
        while let Some(err) = source {
            eprintln!("  Caused by: {err}");
            source = err.source();
        }
        process::exit(1);
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting omni-dev MCP server"
    );

    let service = OmniDevServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
