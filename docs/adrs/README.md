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

| ADR                      | Status      | Date       | Title                                                               |
|--------------------------|-------------|------------|---------------------------------------------------------------------|
| [ADR-0000](adr-0000.md)  | ✅ Accepted | 2026-02-10 | Use Architecture Decision Records                                    |
| [ADR-0001](adr-0001.md)  | ✅ Accepted | 2026-02-10 | YAML as Primary Human Data Exchange Format                           |
| [ADR-0002](adr-0002.md)  | ✅ Accepted | 2026-02-20 | Multi-Provider AI Abstraction via Trait Objects                      |
| [ADR-0003](adr-0003.md)  | ✅ Accepted | 2026-02-20 | Hybrid Git Integration — git2 for Reads, Shell for Complex Mutations |
| [ADR-0004](adr-0004.md)  | ✅ Accepted | 2026-02-21 | Embedded Templates via `include_str!`                                |
| [ADR-0005](adr-0005.md)  | ✅ Accepted | 2026-02-21 | Hierarchical Configuration Resolution with Walk-Up Discovery         |
| [ADR-0006](adr-0006.md)  | ✅ Accepted | 2026-02-22 | Two-View Repository Data Model via Generics and Composition          |
| [ADR-0007](adr-0007.md)  | ✅ Accepted | 2026-02-22 | Preflight Validation Pattern                                         |
| [ADR-0008](adr-0008.md)  | ✅ Accepted | 2026-02-22 | Deterministic Pre-Validation Before AI Analysis                      |
| [ADR-0009](adr-0009.md)  | ✅ Accepted | 2026-02-22 | Token-Budget-Aware Batch Planning                                    |
| [ADR-0010](adr-0010.md)  | ✅ Accepted | 2026-02-22 | Multi-Layer Retry Strategy                                           |
| [ADR-0011](adr-0011.md)  | ✅ Accepted | 2026-02-23 | Compile-Time Model Registry with Identifier Normalization            |
| [ADR-0012](adr-0012.md)  | ✅ Accepted | 2026-02-23 | Three-Level Issue Severity with `--strict` Exit-Code Promotion       |
| [ADR-0013](adr-0013.md)  | ✅ Accepted | 2026-02-23 | Self-Describing YAML Output with Field Presence Tracking             |
| [ADR-0014](adr-0014.md)  | ✅ Accepted | 2026-02-23 | Provider-Specific Prompt Engineering                                 |
