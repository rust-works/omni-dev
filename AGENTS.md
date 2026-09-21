# Repository Guidelines

## Project Structure & Module Organization

`src/lib.rs` exposes the Rust library; `src/main.rs` runs the `omni-dev` CLI. Feature-specific modules live under `src/` (for example, `src/drive/`, `src/git/`, and `src/daemon/`). The optional MCP binary is `src/mcp_server.rs` and requires the `mcp` feature. Integration tests are in `tests/`; unit tests usually sit beside their implementation. Snapshots live in `tests/snapshots/` and module-local `snapshots/` directories. The VS Code extension is in `editors/vscode/`, the site in `website/`, reference assets in `assets/`, and architecture notes and contributor recipes in `docs/`.

## Build, Test, and Development Commands

Use Rust 1.88 or newer. From the repository root:

- `cargo build` builds the default CLI and library.
- `cargo run -- --help` runs the CLI locally.
- `cargo test` runs Rust unit and integration tests; `cargo test --features mcp` also checks the MCP feature.
- `cargo fmt --all -- --check` checks formatting; `cargo clippy --all-targets -- -D warnings` checks lint warnings.
- `./scripts/build.sh` runs the local build, formatting, Clippy, and test checks together.

For extension changes, run `npm ci`, `npm run typecheck`, and `npm test` from `editors/vscode/`.

## Coding Style & Naming Conventions

Follow `rustfmt.toml` (Rust 2021, 100-column width) and `.editorconfig`: four-space indentation for Rust and TOML, two spaces for Markdown, YAML, JSON, and shell. Use `snake_case` for Rust modules, functions, and variables; `PascalCase` for types. Document public APIs with `///`. Keep unsafe code out of ordinary modules; the crate denies it by default.

## Testing Guidelines

Add focused `#[test]` or `#[tokio::test]` cases near changed Rust code and integration tests in `tests/` for CLI behavior. Name tests for the behavior or regression they cover. The suite uses `insta` snapshots and `proptest` where suitable; review snapshot changes before committing. Network-dependent tests are gated separately with `RUSTFLAGS='--cfg online_tests' cargo test`.

## Commit & Pull Request Guidelines

Use scoped conventional commits, such as `feat(drive): add sheet metadata` or `fix(cli): reject invalid input`. Valid types and scopes are listed in `.omni-dev/commit-guidelines.md` and `.omni-dev/scopes.yaml`; CI checks commit messages. In PRs, use `.github/pull_request_template.md`: explain the change and why, link the issue, report tests and coverage, and include screenshots for UI changes. Update user-facing docs and `CHANGELOG.md` for notable changes.

## Worktree Practice

Create new worktrees outside this checkout at `$HOME/wrk/work-trees/omni-dev/<branch>/`. When working in a worktree, target commands and edits at its explicit path, verify `git status --porcelain` there after the first edit, and confirm the branch before committing.
