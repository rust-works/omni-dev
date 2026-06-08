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

| ADR                      | Status                                   | Date       | Title                                                                                 |
|--------------------------|------------------------------------------|------------|---------------------------------------------------------------------------------------|
| [ADR-0000](adr-0000.md)  | ✅ Accepted                              | 2026-02-10 | Use Architecture Decision Records                                                      |
| [ADR-0001](adr-0001.md)  | ✅ Accepted                              | 2026-02-10 | YAML as Primary Human Data Exchange Format                                             |
| [ADR-0002](adr-0002.md)  | ✅ Accepted                              | 2026-02-20 | Multi-Provider AI Abstraction via Trait Objects                                        |
| [ADR-0003](adr-0003.md)  | ✅ Accepted                              | 2026-02-20 | Hybrid Git Integration — git2 for Reads, Shell for Complex Mutations                   |
| [ADR-0004](adr-0004.md)  | ✅ Accepted                              | 2026-02-21 | Embedded Templates via `include_str!`                                                  |
| [ADR-0005](adr-0005.md)  | ✅ Accepted                              | 2026-02-21 | Hierarchical Configuration Resolution with Walk-Up Discovery                           |
| [ADR-0006](adr-0006.md)  | ✅ Accepted                              | 2026-02-22 | Two-View Repository Data Model via Generics and Composition                            |
| [ADR-0007](adr-0007.md)  | ✅ Accepted                              | 2026-02-22 | Preflight Validation Pattern                                                           |
| [ADR-0008](adr-0008.md)  | ✅ Accepted                              | 2026-02-22 | Deterministic Pre-Validation Before AI Analysis                                        |
| [ADR-0009](adr-0009.md)  | ✅ Accepted                              | 2026-02-22 | Token-Budget-Aware Batch Planning                                                      |
| [ADR-0010](adr-0010.md)  | ✅ Accepted                              | 2026-02-22 | Multi-Layer Retry Strategy                                                             |
| [ADR-0011](adr-0011.md)  | 🔄 Superseded by [ADR-0022](adr-0022.md) | 2026-02-23 | Compile-Time Model Registry with Identifier Normalization                              |
| [ADR-0012](adr-0012.md)  | ✅ Accepted                              | 2026-02-23 | Three-Level Issue Severity with `--strict` Exit-Code Promotion                         |
| [ADR-0013](adr-0013.md)  | ✅ Accepted                              | 2026-02-23 | Self-Describing YAML Output with Field Presence Tracking                               |
| [ADR-0014](adr-0014.md)  | ✅ Accepted                              | 2026-02-23 | Provider-Specific Prompt Engineering                                                   |
| [ADR-0015](adr-0015.md)  | ✅ Accepted                              | 2026-02-23 | Dual Error Handling Strategy — `thiserror` for Domain Errors, `anyhow` for Propagation |
| [ADR-0016](adr-0016.md)  | ✅ Accepted                              | 2026-02-24 | Clap Derive Macros with Hierarchical Subcommand Structure                              |
| [ADR-0017](adr-0017.md)  | ✅ Accepted                              | 2026-02-25 | Per-File Diff Splitting for Token Budget Fitting                                       |
| [ADR-0018](adr-0018.md)  | ✅ Accepted                              | 2026-02-25 | Automatic Context Detection for Adaptive AI Prompts                                    |
| [ADR-0019](adr-0019.md)  | ✅ Accepted                              | 2026-02-25 | Ecosystem-Aware Scope Auto-Detection                                                   |
| [ADR-0020](adr-0020.md)  | ✅ Proposed                              | 2026-04-16 | JFM — A Markdown Dialect for Bidirectional ADF Interchange                             |
| [ADR-0021](adr-0021.md)  | ✅ Accepted                              | 2026-04-18 | MCP Server via Second Binary with `rmcp`                                               |
| [ADR-0022](adr-0022.md)  | ✅ Accepted                              | 2026-05-06 | Layered Model Catalog with User and Project Overrides                                  |
| [ADR-0023](adr-0023.md)  | ✅ Accepted                              | 2026-05-10 | Data-Driven ADF Content-Model Schema and Validator                                     |
| [ADR-0024](adr-0024.md)  | ✅ Accepted                              | 2026-05-10 | TTL-Bounded In-Memory Cache for Near-Static JIRA Catalogues                            |
| [ADR-0025](adr-0025.md)  | ✅ Accepted                              | 2026-05-10 | Wire ADF Schema Validator into the API Send Path via `ValidatedAdfDocument`            |
| [ADR-0026](adr-0026.md)  | ✅ Accepted                              | 2026-05-10 | Extending the ADF Validator with Quantifiers, Attributes, and Marks                    |
| [ADR-0027](adr-0027.md)  | ✅ Accepted                              | 2026-05-11 | Destructive CLI Commands Confirm by Default with --force and --dry-run Escape Hatches  |
| [ADR-0028](adr-0028.md)  | ✅ Accepted                              | 2026-05-12 | Sandboxed `claude-cli` Subprocess AI Backend                                           |
| [ADR-0029](adr-0029.md)  | ✅ Accepted                              | 2026-05-12 | JFM ↔ ADF Converter Strategy                                                           |
| [ADR-0030](adr-0030.md)  | ✅ Accepted                              | 2026-05-12 | CLI Snapshot Golden Testing for the Help Surface                                       |
| [ADR-0031](adr-0031.md)  | ✅ Accepted                              | 2026-05-13 | AudioSource Trait Boundary for Real-Time Audio Capture Testability                     |
| [ADR-0032](adr-0032.md)  | ✅ Accepted                              | 2026-05-13 | Separate AudioInput Trait at the Transcriber Boundary                                  |
| [ADR-0033](adr-0033.md)  | ✅ Accepted                              | 2026-05-14 | `candle` as the Production ASR Runtime                                                 |
| [ADR-0034](adr-0034.md)  | ✅ Accepted                              | 2026-05-14 | `tract-onnx` as the Speaker-Embedding Runtime                                          |
| [ADR-0035](adr-0035.md)  | ✅ Accepted                              | 2026-05-25 | OS-Gated ASR Backends with Auto-Upgrading Defaults                                     |
| [ADR-0036](adr-0036.md)  | ✅ Accepted                              | 2026-05-30 | Confused-Deputy Browser Bridge with Dual-Plane Default-Closed Authentication           |
| [ADR-0037](adr-0037.md)  | ✅ Accepted                              | 2026-06-06 | Pure-C Native ASR Backends Behind a Rust FFI Boundary on Non-Windows Targets           |
| [ADR-0038](adr-0038.md)  | ✅ Accepted                              | 2026-06-07 | Streaming `Transcriber` Seam (async `Stream` + `AsyncAudioInput`)                      |
| [ADR-0039](adr-0039.md)  | ✅ Accepted                              | 2026-06-08 | Permit C++/CMake for `mlx-rs` on Apple Silicon — Real-Time INT4 Voxtral via MLX        |
