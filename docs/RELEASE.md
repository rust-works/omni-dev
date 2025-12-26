# Release Process

This document outlines the release process for omni-dev, covering version updates, changelog maintenance, and triggering automated release workflows.

## Overview

The release process follows semantic versioning with **automated CI/CD**:

### Manual Steps (You Do)
1. Version update in `Cargo.toml`
2. Changelog update in `CHANGELOG.md`
3. Code quality checks
4. Commit and push changes
5. Create and push git tag

### Automated Steps (CI Does)
6. GitHub release creation
7. Cross-platform binary builds (Linux, macOS, Windows)
8. crates.io publication
9. Nix binary cache publication

## Prerequisites

Before starting a release, ensure you have:
- [ ] Commit access to the main branch
- [ ] All tests passing locally
- [ ] Clean working directory

## Release Steps

### 1. Version Update

Update the version number in `Cargo.toml`:

```toml
[package]
version = "X.Y.Z"
```

Follow [Semantic Versioning](https://semver.org/):
- **MAJOR** (X): Breaking changes
- **MINOR** (Y): New features (backward compatible)
- **PATCH** (Z): Bug fixes (backward compatible)

### 2. Changelog Update

Update `CHANGELOG.md` following [Keep a Changelog](https://keepachangelog.com/) format:

1. Add new version section with current date:
   ```markdown
   ## [X.Y.Z] - YYYY-MM-DD
   ```

2. Move unreleased changes to the new version section under appropriate categories:
   - **Added**: New features
   - **Changed**: Changes in existing functionality
   - **Deprecated**: Soon-to-be removed features
   - **Removed**: Removed features
   - **Fixed**: Bug fixes
   - **Security**: Security improvements

3. Update version links at the bottom of the changelog:
   ```markdown
   [Unreleased]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z...HEAD
   [X.Y.Z]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z-1...vX.Y.Z
   ```

### 3. Code Quality Checks

Run quality checks to ensure the release is ready:

```bash
# Run tests
cargo test

# Check code quality with clippy
cargo clippy -- -D warnings

# Check formatting
cargo fmt --check

# Build the project
cargo build --release
```

### 4. Commit Changes

Commit the version and changelog updates:

```bash
# Stage changes
git add Cargo.toml CHANGELOG.md

# Commit with conventional commit format
git commit -m "chore: prepare release X.Y.Z

- Update version from X.Y.Z-1 to X.Y.Z
- Update CHANGELOG.md with release notes

🤖 Generated with [Claude Code](https://claude.ai/code)

Co-Authored-By: Claude <noreply@anthropic.com>"
```

### 5. Create Git Tag

Create an annotated tag for the release:

```bash
git tag -a vX.Y.Z -m "Release version X.Y.Z

Features:
- Feature 1
- Feature 2

Fixes:
- Fix 1
- Fix 2
"
```

### 6. Push Changes and Tag

Push the commits and tag to the remote repository:

```bash
# Push commits
git push origin main

# Push tag (this triggers all automated release steps)
git push origin vX.Y.Z
```

## Automated Release Pipeline

Pushing a `v*` tag triggers the following automated workflows:

### CI Workflow (`.github/workflows/ci.yml`)
- Runs tests on stable, beta, and nightly Rust
- Checks formatting with rustfmt
- Runs clippy linting
- Builds documentation
- Generates code coverage
- Builds and tests Nix package
- Publishes to `omni-dev` Cachix binary cache

### Release Workflow (`.github/workflows/release.yml`)
- **Creates GitHub Release**: Automatically from the tag
- **Builds Cross-Platform Binaries**:
  - Linux (x86_64-unknown-linux-gnu)
  - macOS (x86_64-apple-darwin)
  - Windows (x86_64-pc-windows-msvc)
- **Uploads Release Assets**: Attaches compiled binaries to the GitHub release
- **Publishes to crates.io**: Automatically using `CARGO_REGISTRY_TOKEN` secret

### 7. Monitor and Verify

After pushing the tag, monitor the automated release:

1. **Check GitHub Actions**: Watch the release workflow progress
   ```bash
   gh run list --workflow=release.yml
   gh run watch
   ```

2. **Verify GitHub Release**: Check the releases page
   ```bash
   gh release view vX.Y.Z
   ```

3. **Verify crates.io Publication**:
   ```bash
   cargo search omni-dev
   # Or test installation
   cargo install omni-dev
   ```

4. **Verify Nix Binary Cache**:
   ```bash
   nix search github:rust-works/omni-dev
   cachix use omni-dev
   nix profile install github:rust-works/omni-dev
   ```

## Post-Release Tasks

After the automated release completes:

1. **Update Documentation** (if needed):
   - Update any version-specific documentation
   - Ensure README examples use current version

2. **Announce Release** (optional):
   - Share release notes with team
   - Update project status if needed

## Troubleshooting

### Common Issues

**Clippy Warnings**:
```bash
# Fix clippy warnings
cargo clippy --fix --allow-dirty
```

**Test Failures**:
```bash
# Run specific test
cargo test test_name

# Run tests with output
cargo test -- --nocapture
```

**Publication Errors**:
- Ensure crates.io token is configured
- Check for naming conflicts
- Verify all dependencies are published

**Tag Conflicts**:
```bash
# Delete local tag
git tag -d vX.Y.Z

# Delete remote tag
git push --delete origin vX.Y.Z
```

### Rollback Procedure

If a release needs to be rolled back:

1. **GitHub Release**: Mark as pre-release or delete
2. **crates.io**: Cannot delete, but can yank: `cargo yank --vers X.Y.Z`
3. **Git Tag**: Delete and recreate if needed
4. **Version**: Create patch release with fixes

## CI/CD Configuration

The automated release pipeline requires these GitHub secrets:

| Secret                 | Purpose                                                     |
|------------------------|-------------------------------------------------------------|
| `GITHUB_TOKEN`         | Automatically provided by GitHub Actions for release creation |
| `CARGO_REGISTRY_TOKEN` | crates.io API token for publishing                          |
| `CACHIX_AUTH_TOKEN`    | Cachix authentication for Nix binary cache                  |

### Workflow Files

- `.github/workflows/ci.yml` - Quality checks and Nix builds
- `.github/workflows/release.yml` - Release creation and publishing
- `.github/workflows/commit-check.yml` - PR commit message validation

## Security Notes

- Never commit API tokens or credentials
- Use environment variables for sensitive data
- Review all changes before release
- Ensure dependencies are up to date and secure

## References

- [Semantic Versioning](https://semver.org/)
- [Keep a Changelog](https://keepachangelog.com/)
- [Conventional Commits](https://www.conventionalcommits.org/)
- [crates.io Publishing Guide](https://doc.rust-lang.org/cargo/reference/publishing.html)
