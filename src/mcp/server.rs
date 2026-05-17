//! MCP server setup: tool router composition and protocol capabilities.

use std::sync::Arc;

use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{
        Implementation, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
        ProtocolVersion, ReadResourceRequestParams, ReadResourceResult, ServerCapabilities,
        ServerInfo,
    },
    service::RequestContext,
    tool_handler, ErrorData as McpError, RoleServer, ServerHandler,
};

use super::catalogue_cache::CatalogueCache;
use super::resources;

/// The omni-dev MCP server.
///
/// All tool handlers are defined on this struct via `#[tool_router]` in
/// submodules under `src/mcp/`. Routers are combined in [`Self::new`].
#[derive(Clone)]
pub struct OmniDevServer {
    /// Combined tool router.
    pub tool_router: ToolRouter<Self>,
    /// Shared TTL-bounded cache for near-static JIRA catalogue API responses.
    /// Wrapped in `Arc` so cloning the server stays cheap (rmcp clones the
    /// handler per request).
    pub catalogue_cache: Arc<CatalogueCache>,
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
                + Self::atlassian_tool_router()
                + Self::ai_tool_router()
                + Self::config_tool_router()
                + Self::datadog_tool_router(),
            catalogue_cache: Arc::new(CatalogueCache::default()),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OmniDevServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            "omni-dev-mcp",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "omni-dev MCP server. Provides tools for git analysis, commit \
             improvement, and Atlassian integration. Resources expose \
             URI-addressable content via `git://`, `jira://`, `confluence://`, \
             and `omni-dev://` (e.g. `omni-dev://specs/jfm` for the \
             JIRA-Flavoured Markdown reference — fetch before writing JIRA or \
             Confluence content).",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(resources::list_resources_result())
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(resources::list_resource_templates_result())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri;
        let parsed =
            resources::ResourceUri::parse(&uri).map_err(|err| resources::not_found(&uri, err))?;
        resources::read_resource(&parsed, &uri)
            .await
            .map_err(|err| resources::not_found(&uri, err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_advertises_tools_and_resources_capability() {
        let server = OmniDevServer::new();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_some());
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
    fn tool_router_registers_confluence_tools() {
        let server = OmniDevServer::new();
        for name in [
            "confluence_children",
            "confluence_comment_list",
            "confluence_comment_add",
            "confluence_label_list",
            "confluence_label_add",
            "confluence_label_remove",
            "confluence_user_search",
            "confluence_attachment_upload",
            "confluence_attachment_list",
            "confluence_attachment_delete",
            "confluence_space_list",
        ] {
            assert!(
                server.tool_router.has_route(name),
                "expected router to register {name}"
            );
        }
    }

    #[test]
    fn tool_router_lists_all_registered_tools() {
        let server = OmniDevServer::new();
        let tools = server.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in [
            "git_view_commits",
            "git_branch_info",
            "git_check_commits",
            "git_twiddle_commits",
            "git_create_pr",
            "confluence_children",
            "ai_chat",
            "claude_skills_sync",
            "claude_skills_clean",
            "claude_skills_status",
            "config_models_show",
            "atlassian_auth_status",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}; got {names:?}"
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
            "jira_link_list",
            "jira_link_types",
            "jira_link_remove",
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
            "confluence_move",
            "confluence_download",
            "atlassian_convert",
        ] {
            assert!(server.tool_router.has_route(name), "missing: {name}");
        }
    }

    #[test]
    fn tool_router_registers_ai_and_config_tools() {
        let server = OmniDevServer::new();
        for name in [
            "ai_chat",
            "claude_skills_sync",
            "claude_skills_clean",
            "claude_skills_status",
            "config_models_show",
            "atlassian_auth_status",
        ] {
            assert!(
                server.tool_router.has_route(name),
                "router missing route {name}"
            );
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
        assert!(from_default.tool_router.has_route("confluence_children"));
    }

    #[test]
    fn tool_router_registers_all_datadog_tools() {
        let server = OmniDevServer::new();
        for name in [
            "datadog_auth_status",
            "datadog_metrics_query",
            "datadog_monitor_list",
            "datadog_monitor_get",
            "datadog_monitor_search",
            "datadog_dashboard_list",
            "datadog_dashboard_get",
            "datadog_logs_search",
        ] {
            assert!(server.tool_router.has_route(name), "missing route: {name}");
        }
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
            "jira_transition_list",
            "jira_comment",
            "jira_comment_edit",
            "jira_link",
            "jira_dev",
            "jira_user_search",
        ] {
            assert!(server.tool_router.has_route(name), "missing tool: {name}");
        }
    }
}
