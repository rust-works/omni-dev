# Release Process

This document outlines the comprehensive release process for omni-dev, covering version updates, changelog maintenance, GitHub releases, and crates.io publication.

## Overview

The release process follows semantic versioning and includes:
1. Version updates
2. Changelog maintenance
3. Code quality checks
4. Git tagging
5. GitHub release creation
6. crates.io publication
7. Nix binary cache publication

## Prerequisites

Before starting a release, ensure you have:
- [ ] Commit access to the main branch
- [ ] GitHub CLI (`gh`) installed and authenticated
- [ ] Cargo configured with crates.io API token
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

# Push tag (this triggers CI to build and publish to Nix binary cache)
git push origin vX.Y.Z
```

**Note**: Pushing the tag automatically triggers CI to:
- Run all quality checks
- Build the Nix package
- Publish to the `omni-dev` binary cache on Cachix

### 7. Create GitHub Release

Create a GitHub release using the GitHub CLI:

```bash
gh release create vX.Y.Z \
  --title "Release vX.Y.Z" \
  --notes-from-tag
```

Alternatively, create a release with custom notes:

```bash
gh release create vX.Y.Z \
  --title "Release vX.Y.Z" \
  --notes "Release notes here..."
```

The release notes should include:
- 🚀 **Features**: New functionality
- 🔄 **Changes**: Modifications to existing features
- 🐛 **Fixes**: Bug fixes and improvements
- Link to full changelog

### 8. Publish to crates.io

Publish the package to crates.io:

```bash
cargo publish
```

This will:
- Package the crate
- Verify the build
- Upload to crates.io registry
- Make it available for `cargo install omni-dev`

### 9. Verify Nix Binary Cache Publication

Check that the Nix binary cache was successfully updated:

1. **Monitor CI**: Check that the tag-triggered CI run completed successfully
2. **Verify cache contents**:
   ```bash
   # Check if the release is available in the cache
   nix search github:rust-works/omni-dev

   # Test installation from cache
   cachix use omni-dev
   nix profile install github:rust-works/omni-dev
   ```

The binary cache publication happens automatically when you push the tag, making Nix installations much faster for users.

## Post-Release Tasks

After completing the release:

1. **Verify Publication**:
   - Check GitHub releases page
   - Verify crates.io listing
   - Test installation: `cargo install omni-dev`
   - Verify Nix binary cache: `nix profile install github:rust-works/omni-dev`

2. **Update Documentation**:
   - Update any version-specific documentation
   - Ensure README examples use current version

3. **Announce Release**:
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

## Automation Considerations

For future automation, consider:
- GitHub Actions for automated releases
- Automated changelog generation
- Version bump automation
- Integration tests in CI/CD

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
