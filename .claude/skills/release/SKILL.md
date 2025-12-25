---
name: release
description: Automates the release process for this Rust project. Use when creating a new release, preparing a version bump, or publishing to GitHub/crates.io. Triggers on terms like "release", "publish", "version bump", "create tag".
---

# Release Process Skill

This skill guides the complete release process for omni-dev following the documented procedure in [docs/RELEASE.md](../../../docs/RELEASE.md).

## Release Checklist

1. **Version Update** - Update version in `Cargo.toml`
2. **Changelog Update** - Move `[Unreleased]` content to new version section with date
3. **Version Links** - Update comparison links at bottom of CHANGELOG.md
4. **Quality Checks** - Run `cargo test`, `cargo clippy -- -D warnings`, `cargo build --release`
5. **Commit** - Create release commit with conventional format
6. **Tag** - Create annotated git tag `vX.Y.Z`
7. **Push** - Push commits and tag to remote (CI handles the rest)

## What CI Handles Automatically

When you push a tag, CI will automatically:
- Run all quality checks
- Build the Nix package
- Publish to the `omni-dev` binary cache on Cachix
- Create the GitHub release

**Do NOT manually run `gh release create`** - let CI handle it to avoid conflicts.

## Manual Steps Only

Only these steps require manual intervention:
- Version bump in Cargo.toml
- Changelog updates
- Commit and tag creation
- Push to remote
- (Optional) `cargo publish` for crates.io

## Key Learnings

### Let CI Create Releases
Avoid creating GitHub releases manually with `gh release create`. The CI workflow handles this automatically when you push a tag. Manual creation causes CI to fail with "Resource not accessible" or duplicate release errors.

### Changelog Management
The `[Unreleased]` section should be populated incrementally as features merge, not all at once during release.

### Version Link Updates
When releasing, update BOTH:
- The `[Unreleased]` comparison link (point to new version tag)
- Add the new version's comparison link

## Commit Message Format

```
chore: prepare release vX.Y.Z

- Update version from X.Y.Z-1 to X.Y.Z
- Update CHANGELOG.md with release notes

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
```

## Commands Reference

```bash
# Quality checks
cargo test
cargo clippy -- -D warnings
cargo build --release

# Git operations
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore: prepare release vX.Y.Z..."
git tag -a vX.Y.Z -m "Release version X.Y.Z..."
git push origin main
git push origin vX.Y.Z

# Publish to crates.io (optional, manual)
cargo publish
```
