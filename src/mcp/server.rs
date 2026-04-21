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
            tool_router: Self::git_tool_router()
                + Self::jira_tool_router()
                + Self::jira_core_tool_router()
                + Self::confluence_tool_router()
                + Self::atlassian_tool_router(),
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
    fn tool_router_registers_all_phase1_git_tools() {
        let server = OmniDevServer::new();
        for name in [
            "git_view_commits",
            "git_branch_info",
            "git_check_commits",
            "git_twiddle_commits",
            "git_create_pr",
        ] {
            assert!(server.tool_router.has_route(name), "missing route: {name}");
        }
    }

    #[test]
    fn tool_router_lists_all_registered_tools() {
        let server = OmniDevServer::new();
        let tools = server.tool_router.list_all();
        let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in [
            "git_view_commits",
            "git_branch_info",
            "git_check_commits",
            "git_twiddle_commits",
            "git_create_pr",
        ] {
            assert!(
                names.contains(&expected),
                "missing: {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn tool_router_registers_all_jira_extension_tools() {
        let server = OmniDevServer::new();
        let expected = [
            "jira_attachment_download",
            "jira_attachment_images",
            "jira_board_list",
            "jira_board_issues",
            "jira_changelog",
            "jira_delete",
            "jira_field_list",
            "jira_field_options",
            "jira_project_list",
            "jira_sprint_list",
            "jira_sprint_issues",
            "jira_sprint_add",
            "jira_sprint_create",
            "jira_sprint_update",
            "jira_watcher_list",
            "jira_watcher_add",
            "jira_watcher_remove",
            "jira_worklog_list",
            "jira_worklog_add",
        ];
        for name in expected {
            assert!(server.tool_router.has_route(name), "missing route: {name}");
        }
    }

    #[test]
    fn tool_router_registers_all_confluence_and_atlassian_tools() {
        let server = OmniDevServer::new();
        for name in [
            "confluence_read",
            "confluence_search",
            "confluence_create",
            "confluence_write",
            "confluence_delete",
            "confluence_download",
            "atlassian_convert",
        ] {
            assert!(server.tool_router.has_route(name), "missing: {name}");
        }
    }

    #[test]
    fn default_constructs_same_as_new() {
        let from_default = OmniDevServer::default();
        let from_new = OmniDevServer::new();
        assert_eq!(
            from_default.tool_router.list_all().len(),
            from_new.tool_router.list_all().len(),
        );
        assert!(from_default.tool_router.has_route("git_view_commits"));
    }

    #[test]
    fn tool_router_registers_all_jira_tools() {
        let server = OmniDevServer::new();
        for name in [
            "jira_read",
            "jira_search",
            "jira_create",
            "jira_write",
            "jira_transition",
            "jira_comment",
            "jira_link",
            "jira_dev",
        ] {
            assert!(server.tool_router.has_route(name), "missing tool: {name}");
        }
    }
}
