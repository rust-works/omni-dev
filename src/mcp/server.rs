//! MCP server setup: tool router composition and protocol capabilities.

use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool_handler, ServerHandler,
};

/// The omni-dev MCP server.
///
/// All tool handlers are defined on this struct via `#[tool_router]` in
/// submodules under `src/mcp/`. Routers are combined in [`Self::new`].
#[derive(Clone)]
pub struct OmniDevServer {
    /// Combined tool router.
    pub tool_router: ToolRouter<Self>,
}

impl Default for OmniDevServer {
    fn default() -> Self {
        Self::new()
    }
}

impl OmniDevServer {
    /// Constructs a new server with all tool routers combined.
    pub fn new() -> Self {
        Self {
            tool_router: Self::git_tool_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OmniDevServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "omni-dev-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "omni-dev MCP server. Provides tools for git analysis, commit \
                 improvement, and Atlassian integration.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_advertises_tools_capability() {
        let server = OmniDevServer::new();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "omni-dev-mcp");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn tool_router_registers_git_view_commits() {
        let server = OmniDevServer::new();
        assert!(server.tool_router.has_route("git_view_commits"));
    }

    #[test]
    fn tool_router_lists_all_registered_tools() {
        let server = OmniDevServer::new();
        let tools = server.tool_router.list_all();
        assert!(tools.iter().any(|t| t.name.as_ref() == "git_view_commits"));
    }
}
