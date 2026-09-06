# Architecture Decision Records

This directory contains the Architecture Decision Records (ADRs) for the omni-dev project.

An ADR is a short document that captures a single significant architectural or design decision
along with its context and consequences. ADRs give current and future contributors a way to
understand *why* the system is shaped the way it is, not just *how* it works.

For more background on the practice, see
[Documenting Architecture Decisions](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
by Michael Nygard.

## Status Legend

| Emoji | Status     | Meaning                               |
|-------|------------|---------------------------------------|
| 🟡    | Proposed   | Under discussion, not yet agreed upon  |
| ✅    | Accepted   | Agreed and in effect                   |
| ❌    | Deprecated | No longer applies                      |
| 🔄    | Superseded | Replaced by a newer ADR                |

## Inventory

| ADR                      | Status                                   | Date       | Title                                                                                       |
|--------------------------|------------------------------------------|------------|---------------------------------------------------------------------------------------------|
| [ADR-0000](adr-0000.md)  | ✅ Accepted                              | 2026-02-10 | Use Architecture Decision Records                                                           |
| [ADR-0001](adr-0001.md)  | ✅ Accepted                              | 2026-02-10 | YAML as Primary Human Data Exchange Format                                                  |
| [ADR-0002](adr-0002.md)  | ✅ Accepted                              | 2026-02-20 | Multi-Provider AI Abstraction via Trait Objects                                             |
| [ADR-0003](adr-0003.md)  | ✅ Accepted                              | 2026-02-20 | Hybrid Git Integration — git2 for Reads, Shell for Complex Mutations                        |
| [ADR-0004](adr-0004.md)  | ✅ Accepted                              | 2026-02-21 | Embedded Templates via `include_str!`                                                       |
| [ADR-0005](adr-0005.md)  | ✅ Accepted                              | 2026-02-21 | Hierarchical Configuration Resolution with Walk-Up Discovery                                |
| [ADR-0006](adr-0006.md)  | ✅ Accepted                              | 2026-02-22 | Two-View Repository Data Model via Generics and Composition                                 |
| [ADR-0007](adr-0007.md)  | ✅ Accepted                              | 2026-02-22 | Preflight Validation Pattern                                                                |
| [ADR-0008](adr-0008.md)  | ✅ Accepted                              | 2026-02-22 | Deterministic Pre-Validation Before AI Analysis                                             |
| [ADR-0009](adr-0009.md)  | ✅ Accepted                              | 2026-02-22 | Token-Budget-Aware Batch Planning                                                           |
| [ADR-0010](adr-0010.md)  | ✅ Accepted                              | 2026-02-22 | Multi-Layer Retry Strategy                                                                  |
| [ADR-0011](adr-0011.md)  | 🔄 Superseded by [ADR-0022](adr-0022.md) | 2026-02-23 | Compile-Time Model Registry with Identifier Normalization                                   |
| [ADR-0012](adr-0012.md)  | ✅ Accepted                              | 2026-02-23 | Three-Level Issue Severity with `--strict` Exit-Code Promotion                              |
| [ADR-0013](adr-0013.md)  | ✅ Accepted                              | 2026-02-23 | Self-Describing YAML Output with Field Presence Tracking                                    |
| [ADR-0014](adr-0014.md)  | ✅ Accepted                              | 2026-02-23 | Provider-Specific Prompt Engineering                                                        |
| [ADR-0015](adr-0015.md)  | ✅ Accepted                              | 2026-02-23 | Dual Error Handling Strategy — `thiserror` for Domain Errors, `anyhow` for Propagation      |
| [ADR-0016](adr-0016.md)  | ✅ Accepted                              | 2026-02-24 | Clap Derive Macros with Hierarchical Subcommand Structure                                   |
| [ADR-0017](adr-0017.md)  | ✅ Accepted                              | 2026-02-25 | Per-File Diff Splitting for Token Budget Fitting                                            |
| [ADR-0018](adr-0018.md)  | ✅ Accepted                              | 2026-02-25 | Automatic Context Detection for Adaptive AI Prompts                                         |
| [ADR-0019](adr-0019.md)  | ✅ Accepted                              | 2026-02-25 | Ecosystem-Aware Scope Auto-Detection                                                        |
| [ADR-0020](adr-0020.md)  | ✅ Accepted                              | 2026-04-16 | JFM — A Markdown Dialect for Bidirectional ADF Interchange                                  |
| [ADR-0021](adr-0021.md)  | ✅ Accepted                              | 2026-04-18 | MCP Server via Second Binary with `rmcp`                                                    |
| [ADR-0022](adr-0022.md)  | ✅ Accepted                              | 2026-05-06 | Layered Model Catalog with User and Project Overrides                                       |
| [ADR-0023](adr-0023.md)  | ✅ Accepted                              | 2026-05-10 | Data-Driven ADF Content-Model Schema and Validator                                          |
| [ADR-0024](adr-0024.md)  | ✅ Accepted                              | 2026-05-10 | TTL-Bounded In-Memory Cache for Near-Static JIRA Catalogues                                 |
| [ADR-0025](adr-0025.md)  | ✅ Accepted                              | 2026-05-10 | Wire ADF Schema Validator into the API Send Path via `ValidatedAdfDocument`                 |
| [ADR-0026](adr-0026.md)  | ✅ Accepted                              | 2026-05-10 | Extending the ADF Validator with Quantifiers, Attributes, and Marks                         |
| [ADR-0027](adr-0027.md)  | ✅ Accepted                              | 2026-05-11 | Destructive CLI Commands Confirm by Default with --force and --dry-run Escape Hatches       |
| [ADR-0028](adr-0028.md)  | ✅ Accepted                              | 2026-05-12 | Sandboxed `claude-cli` Subprocess AI Backend                                                |
| [ADR-0029](adr-0029.md)  | ✅ Accepted                              | 2026-05-12 | JFM ↔ ADF Converter Strategy                                                                |
| [ADR-0030](adr-0030.md)  | ✅ Accepted                              | 2026-05-12 | CLI Snapshot Golden Testing for the Help Surface                                            |
| [ADR-0031](adr-0031.md)  | 🔄 Superseded by [ADR-0038](adr-0038.md) | 2026-05-13 | AudioSource Trait Boundary for Real-Time Audio Capture Testability                          |
| [ADR-0032](adr-0032.md)  | 🔄 Superseded by [ADR-0038](adr-0038.md) | 2026-05-13 | Separate AudioInput Trait at the Transcriber Boundary                                       |
| [ADR-0033](adr-0033.md)  | 🔄 Superseded by [ADR-0038](adr-0038.md) | 2026-05-14 | `candle` as the Production ASR Runtime                                                      |
| [ADR-0034](adr-0034.md)  | 🔄 Superseded by [ADR-0038](adr-0038.md) | 2026-05-14 | `tract-onnx` as the Speaker-Embedding Runtime                                               |
| [ADR-0035](adr-0035.md)  | 🔄 Superseded by [ADR-0038](adr-0038.md) | 2026-05-25 | OS-Gated ASR Backends with Auto-Upgrading Defaults                                          |
| [ADR-0036](adr-0036.md)  | ✅ Accepted                              | 2026-05-30 | Confused-Deputy Browser Bridge with Dual-Plane Default-Closed Authentication                |
| [ADR-0037](adr-0037.md)  | 🔄 Superseded by [ADR-0038](adr-0038.md) | 2026-06-06 | Pure-C Native ASR Backends Behind a Rust FFI Boundary on Non-Windows Targets                |
| [ADR-0038](adr-0038.md)  | ✅ Accepted                              | 2026-06-13 | Voice Functionality Extracted to the omni-voice Repository                                  |
| [ADR-0039](adr-0039.md)  | ✅ Accepted                              | 2026-06-18 | Extensible omni-dev Daemon Hosting Pluggable Services over a Unix Control Socket            |
| [ADR-0040](adr-0040.md)  | ✅ Accepted                              | 2026-06-23 | Cross-Window Worktrees Daemon Service Fed by a Companion VS Code Extension                  |
| [ADR-0041](adr-0041.md)  | ✅ Accepted                              | 2026-07-07 | Refuse to Amend Pushed Commits; Override Denied Only to Autonomous AI Rewrites              |
| [ADR-0042](adr-0042.md)  | ✅ Accepted                              | 2026-07-07 | Local Append-Only Invocation and HTTP Request Log                                           |
| [ADR-0043](adr-0043.md)  | ✅ Accepted                              | 2026-07-07 | Default-On Denylist Redaction of Credential Material in Persisted and Debug Output          |
| [ADR-0044](adr-0044.md)  | ✅ Accepted                              | 2026-07-07 | Unified AI Backend and Model Resolution with a Single-Source-of-Truth Precedence Chain      |
| [ADR-0045](adr-0045.md)  | ✅ Accepted                              | 2026-07-07 | Isolated Named Credential Profiles for Multi-Tenant Configuration                           |
| [ADR-0046](adr-0046.md)  | ✅ Accepted                              | 2026-07-07 | Unified `-o/--output <format>` Convention with Dedicated `--out-file` for File Destinations |
| [ADR-0047](adr-0047.md)  | ✅ Accepted                              | 2026-07-07 | Remote-First Default Base-Branch Resolution That Fails Closed                               |
| [ADR-0048](adr-0048.md)  | ✅ Accepted                              | 2026-07-10 | Repo/Worktree Tree View Fed by a Daemon Push Subscription                                   |
| [ADR-0049](adr-0049.md)  | ✅ Accepted                              | 2026-07-10 | Destructive Worktree Close over the Daemon, Guarded by Structural `is_main`                 |
| [ADR-0050](adr-0050.md)  | 🔄 Superseded                            | 2026-07-13 | Worktree PR Badges Resolved Extension-Side via `gh`, Not the Daemon                         |
| [ADR-0051](adr-0051.md)  | ✅ Accepted                              | 2026-07-14 | Do Not Pre-Trust Daemon-Opened Worktree Windows; Recommend Parent-Folder Trust              |
| [ADR-0052](adr-0052.md)  | ✅ Accepted                              | 2026-07-15 | Cross-Window Claude Code Sessions Tracker as a Daemon Service Fed by Three Feeds            |
| [ADR-0053](adr-0053.md)  | ✅ Accepted                              | 2026-07-15 | Worktree PR Badges Resolved in the Daemon via `gh api graphql`                              |
| [ADR-0054](adr-0054.md)  | ✅ Accepted                              | 2026-07-20 | Native Windows Daemon Control Plane on a Per-User Named Pipe with Detached-Spawn Activation |
| [ADR-0055](adr-0055.md)  | ✅ Accepted                              | 2026-07-24 | Batch Worktree Rebase onto Remote Main, Fetch-Once-Per-Repository, CLI-Side                 |
| [ADR-0056](adr-0056.md)  | ✅ Accepted                              | 2026-07-24 | Batch Merge-Queue Enqueue over the Daemon, Gated by Server-Side Eligibility                 |
| [ADR-0057](adr-0057.md)  | ✅ Accepted                              | 2026-07-25 | Authoritative Claude Session State via a Stream-Tee Process Wrapper                         |
| [ADR-0058](adr-0058.md)  | ✅ Accepted                              | 2026-07-25 | Repositioning VS Code Windows from the Daemon via the macOS Accessibility API               |
| [ADR-0059](adr-0059.md)  | ✅ Accepted                              | 2026-07-26 | Daemon-Hosted Worktree Rebase, Conflicts Left In Place                                      |
| [ADR-0060](adr-0060.md)  | ✅ Accepted                              | 2026-07-28 | The Main Working Tree Is a Valid Batch-Rebase Target                                        |
| [ADR-0061](adr-0061.md)  | ✅ Accepted                              | 2026-07-30 | Daemon-Hosted Force-Push With a Lease                                                       |
| [ADR-0062](adr-0062.md)  | ✅ Accepted                              | 2026-08-01 | Drop the VS Code Rebase Confirmation Modal                                                  |
| [ADR-0063](adr-0063.md)  | ✅ Accepted                              | 2026-08-01 | OAuth2 Authorization-Code + PKCE for Gmail, with Bring-Your-Own Google Cloud Project        |
| [ADR-0064](adr-0064.md)  | ✅ Accepted                              | 2026-08-03 | Presence-on-Disk Idempotence and Immutable-`.eml`/Mutable-Manifest Split for `gmail sync`   |
| [ADR-0065](adr-0065.md)  | ✅ Accepted                              | 2026-08-04 | `gmail sync --extract-attachments`                                                          |
| [ADR-0066](adr-0066.md)  | ✅ Accepted                              | 2026-08-05 | A Gmail-Specific Named-Account Store, Orthogonal to `--profile`                             |
| [ADR-0067](adr-0067.md)  | ✅ Accepted                              | 2026-08-06 | Automatic Chrome-Profile Resolution for Gmail Login, Opt-In Per Account                     |
| [ADR-0068](adr-0068.md)  | ✅ Accepted                              | 2026-08-06 | Concurrent Multi-Account Gmail Sync via `gmail-sync.yaml` and a Shared Fetch Semaphore      |
| [ADR-0069](adr-0069.md)  | ✅ Accepted                              | 2026-08-15 | A Drive-Specific Named-Account Store and Read-Only OAuth2 Client, Mirroring Gmail's Design  |
| [ADR-0070](adr-0070.md)  | ✅ Accepted                              | 2026-08-18 | Security-Gated Rename/Move for the Drive Integration                                        |
| [ADR-0071](adr-0071.md)  | ✅ Accepted                              | 2026-08-25 | Folder-Scoped Write Permissions for the Drive Integration                                   |
| [ADR-0072](adr-0072.md)  | ✅ Accepted                              | 2026-09-05 | A Terminal UI for the Worktrees View                                                        |
| [ADR-0073](adr-0073.md)  | ✅ Accepted                              | 2026-09-06 | Google Sheets API Support for the Drive Integration                                         |
| [ADR-0074](adr-0074.md)  | ✅ Accepted                              | 2026-09-06 | File-Id-Keyed Write-Permission Rules                                                        |
