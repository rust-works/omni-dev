//! Binary entry point for the omni-dev MCP server.
//!
//! Speaks the Model Context Protocol over stdio so AI assistants can
//! invoke omni-dev's business logic as MCP tools. See [ADR-0021].
//!
//! All non-trivial logic lives in `omni_dev::mcp::runtime` so it can be
//! exercised by library tests; this binary is intentionally a thin shim.

use std::process;

use omni_dev::mcp;
use rmcp::transport::stdio;

#[tokio::main]
async fn main() {
    let _ = mcp::try_init_tracing();
    mcp::log_startup_event();
    if let Err(e) = mcp::serve_with(stdio()).await {
        let mut stderr = std::io::stderr().lock();
        let _ = mcp::write_error_chain(&mut stderr, &e);
        process::exit(1);
    }
}
