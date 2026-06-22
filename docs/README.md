# omni-dev Documentation

Complete documentation for omni-dev - the intelligent Git commit message toolkit with AI-powered contextual intelligence.

## 📚 User Documentation

### Getting Started

- **[Getting Started](getting-started.md)** — start here (10-minute
  walkthrough from install to first AI-improved commit)
- **[README](../README.md)** - Quick overview and installation
- **[User Guide](user-guide.md)** - Comprehensive usage guide with examples
- **[Configuration Guide](configuration.md)** - Set up contextual intelligence
- **[Local Overrides](local-overrides.md)** - Personal configuration customization
- **[Configuration Best Practices](configuration-best-practices.md)** - Writing effective scopes and guidelines
- **[AI Backends](ai-backends.md)** - Claude API, Claude CLI, OpenAI, Ollama, and Bedrock setup
- **[Examples](examples.md)** - Real-world usage examples across different project types

### Atlassian Integration

- **[User Guide - Atlassian](user-guide.md#atlassian---jira-and-confluence-integration)** - JIRA and Confluence commands
- **[JFM Specification](specs/jfm.md)** - JIRA-Flavored Markdown format and ADF conversion

### Datadog Integration

- **[Datadog Guide](datadog.md)** - Authentication, metrics, monitors, dashboards, logs, events, SLOs, downtimes, hosts; CLI and MCP forms

### Browser Bridge

- **[Browser Bridge Guide](browser-bridge.md)** - Drive authenticated HTTP requests through a browser tab (Grafana/Loki, internal dashboards); the two planes, the `request` thin client, and the security model
- **[Recipe: querying your own Facebook data](recipes/browser-bridge-facebook.md)** - Page your own Facebook timeline via internal Relay/GraphQL; the worked example for the `--allow-origin` + `--credentials omit` cross-origin flags
- **[ADR-0036](adrs/adr-0036.md)** - Confused-deputy trust boundary and dual-plane default-closed authentication

### Daemon

- **[Running under the daemon](browser-bridge.md#running-under-the-daemon)** - Host long-lived services (the browser bridge, Snowflake) in one supervised process behind a per-user Unix-domain control socket; `daemon run/start/stop/restart/status` and the optional macOS menu-bar app
- **[ADR-0039](adrs/adr-0039.md)** - Daemon architecture: service abstraction, Unix-socket control plane, single-instance supervision, and lifecycle

### Snowflake Integration

- **[Snowflake Service](snowflake-service.md)** - Authenticate once via external-browser SSO, then run concurrent SQL across any account through the daemon's multiplexed session pool; `omni-dev snowflake query/sessions/disconnect`

### Request Log

- **[Request Log](log.md)** - The local, append-only invocation + HTTP log and the `omni-dev log` reader (filter matrix, query mini-language, `--follow`); central header redaction and the `OMNI_DEV_LOG_*` opt-ins/opt-out

### MCP Server

- **[MCP Reference](mcp.md)** - Tool catalog, resources, and setup for Claude Desktop / Claude Code / MCP Inspector
- **[Glama Listing](glama-listing.md)** - Admin-form configuration and per-release publication procedure for the [glama.ai](https://glama.ai/mcp/servers/rust-works/omni-dev) listing
- **[ADR-0021](adrs/adr-0021.md)** - Architectural decision behind the second binary

### Transcript Fetching

- **[Transcript Reference](transcript.md)** - `omni-dev transcript` CLI reference, library architecture, and the recipe for adding a new source

### Reference & Support

- **[Shell Completion](shell-completion.md)** - Install per-shell completion scripts for `bash`, `zsh`, `fish`, `powershell`, and `elvish`
- **[Troubleshooting](troubleshooting.md)** - Common issues and solutions  
- **[API Documentation](https://docs.rs/omni-dev)** - Rust API reference
- **[Changelog](../CHANGELOG.md)** - Version history and changes

## 🛠️ Developer Documentation

### Project Planning & Architecture

Each file in [`plan/`](plan/) carries a `**Status:**` header (`Built`, `In Progress`, `Aspirational`, or `Historical`) and may cross-link the ADRs that capture its current architecture. Use a plan to **explore** a design before high-level decisions stabilise; promote it to an [ADR](adrs/README.md) once the decisions firm up; document the *current* user-facing behaviour in [user-guide.md](user-guide.md). Once a plan's decisions are fully captured by ADRs, change its status to `Built` (with ADR cross-links) or `Historical` rather than deleting it — the prior reasoning stays discoverable. See [STYLE-0027](STYLE_GUIDE.md#style-0027-plan-file-status-header-and-adr-cross-links) for the full convention.

- **[Project Plan](plan/project.md)** *(Historical)* - Initial CLI architecture predating Atlassian/MCP/AI providers
- **[Twiddle Design](plan/twiddle.md)** *(In Progress)* - Phases 1 and 2 built; Phase 3 (contextual intelligence) aspirational
- **[Help All Command](plan/help-all-command.md)** *(Built)* - Comprehensive help system design
- **[Config Internals](plan/config-internals.md)** *(Built, canonical reference)* - How configuration resolution works · [ADR-0005](adrs/adr-0005.md) · [ADR-0018](adrs/adr-0018.md) · [ADR-0019](adrs/adr-0019.md)
- **[AI Client](plan/AiClient.md)** *(Built)* - Multi-provider AI abstraction · [ADR-0002](adrs/adr-0002.md) · [ADR-0014](adrs/adr-0014.md)
- **[Commit Message Check](plan/commit-message-check.md)** *(Built)* - Non-interactive commit message validation

### Retrospectives

- **[v0.18.0 Retrospective](retrospective-v0.18.0.md)** - ADR-guided code quality and issue-driven development

### Development & Release

- **[Contributing Guidelines](../CONTRIBUTING.md)** - How to contribute to the project
- **[Release Process](RELEASE.md)** - Complete release workflow and procedures

## 🚀 Quick Navigation

### I want to

**Get started quickly**
→ [Getting Started](getting-started.md) → [README Quick Start](../README.md#-quick-start)

**Understand all features**  
→ [User Guide](user-guide.md) → [Core Commands](user-guide.md#command-reference)

**Set up my project**
→ [Configuration Guide](configuration.md) → [Project Configuration](configuration.md#setting-up-context)

**See real examples**
→ [Examples](examples.md) → [Your Project Type](examples.md#table-of-contents)

**Write better configuration**
→ [Best Practices](configuration-best-practices.md) → [Scope Guidelines](configuration-best-practices.md#writing-effective-scope-definitions)

**Fix a problem**
→ [Troubleshooting](troubleshooting.md) → [Common Solutions](troubleshooting.md#common-solutions-checklist)

**Contribute to the project**
→ [Contributing Guidelines](../CONTRIBUTING.md) → [Development Setup](../CONTRIBUTING.md#development-setup)

## 📖 Documentation Overview

### User Journey

1. **Discovery**: Start with [README](../README.md) for overview and quick installation
2. **First run**: Follow [Getting Started](getting-started.md) for a 10-minute walkthrough
3. **Learning**: Continue with [User Guide](user-guide.md) for comprehensive usage
4. **Setup**: Use [Configuration Guide](configuration.md) to set up your project
5. **Practice**: Try [Examples](examples.md) relevant to your project type
6. **Troubleshooting**: Reference [Troubleshooting](troubleshooting.md) when needed

### Key Features Covered

| Feature | Primary Documentation | Additional Resources |
|---------|----------------------|----------------------|
| **AI-Powered Twiddle** | [User Guide - Twiddle](user-guide.md#twiddle---ai-powered-improvement) | [Examples - Before/After](examples.md#beforeafter-showcases) |
| **AI Backends** | [AI Backends Guide](ai-backends.md) | [README - AI backend selection](../README.md#ai-backend-selection) |
| **Contextual Intelligence** | [Configuration Guide](configuration.md#contextual-intelligence) | [User Guide - Context](user-guide.md#contextual-intelligence) |
| **Project Configuration** | [Configuration Guide - Setup](configuration.md#setting-up-context) | [Examples - Configurations](examples.md#project-specific-examples) |
| **Local Overrides** | [Local Overrides Guide](local-overrides.md) | [Configuration - Local Setup](configuration.md#local-override-examples) |
| **Automatic Batching** | [User Guide - Batching](user-guide.md#automatic-batching) | [Troubleshooting - Performance](troubleshooting.md#performance-issues) |
| **Workflow Integration** | [User Guide - Workflows](user-guide.md#workflows) | [Examples - Enterprise](examples.md#enterprise-monorepo) |
| **Configuration Quality** | [Best Practices](configuration-best-practices.md) | [Config Internals](plan/config-internals.md) |
| **Atlassian Integration** | [User Guide - Atlassian](user-guide.md#atlassian---jira-and-confluence-integration) | [JFM Spec](specs/jfm.md) |
| **Datadog Integration** | [Datadog Guide](datadog.md) | [User Guide - Datadog](user-guide.md#datadog-integration) · [README - Datadog](../README.md#-datadog-integration-read-only) |
| **MCP Server** | [MCP Reference](mcp.md) | [README - MCP Server](../README.md#-mcp-server) |

## 🔗 External Resources

- **[GitHub Repository](https://github.com/rust-works/omni-dev)** - Source code and issues
- **[omni-dev-commit-check Action](https://github.com/action-works/omni-dev-commit-check)** - GitHub Action for CI commit validation
- **[Crates.io Page](https://crates.io/crates/omni-dev)** - Package information
- **[Rust API Docs](https://docs.rs/omni-dev)** - Generated API documentation
- **[GitHub Discussions](https://github.com/rust-works/omni-dev/discussions)** - Community support
- **[Anthropic Console](https://console.anthropic.com/)** - Get your Claude API key

## 🤝 Community

- **Questions**: Use [GitHub Discussions](https://github.com/rust-works/omni-dev/discussions)
- **Bug Reports**: Open an [Issue](https://github.com/rust-works/omni-dev/issues)  
- **Feature Requests**: Start a [Discussion](https://github.com/rust-works/omni-dev/discussions) first
- **Contributions**: See [Contributing Guidelines](../CONTRIBUTING.md)

## 📅 Documentation Maintenance

This documentation is maintained alongside the codebase and updated with each release. If you find any issues or have suggestions for improvement:

1. **Minor fixes**: Submit a pull request directly
2. **Major changes**: Open an issue for discussion first
3. **New examples**: Contributions welcome via pull request

---

**Need help choosing where to start?**

- **New to omni-dev**: [Getting Started](getting-started.md) → [User Guide](user-guide.md)
- **Setting up a project**: [Configuration Guide](configuration.md)
- **Having issues**: [Troubleshooting](troubleshooting.md)
- **Want examples**: [Examples](examples.md)
- **Contributing**: [Contributing Guidelines](../CONTRIBUTING.md)
