---
name: release
description: Automates the release process for this Rust project. Use when creating a new release, preparing a version bump, or publishing to GitHub/crates.io. Triggers on terms like "release", "publish", "version bump", "create tag".
---

# Automated Release Skill

This skill performs the complete end-to-end release process for omni-dev, from version bump to verified publication.

## Execution Steps

### Phase 1: Preparation

1. **Verify Clean State**
   ```bash
   git status --porcelain
   ```
   Abort if working directory is not clean.

2. **Get Current Version**
   ```bash
   grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'
   ```

3. **Get Last Release Tag**
   ```bash
   git describe --tags --abbrev=0
   ```

4. **Analyze Changes Since Last Release**
   ```bash
   git log --oneline $(git describe --tags --abbrev=0)..HEAD
   ```

5. **Determine Version Bump**
   - MAJOR: Breaking changes (removed APIs, changed signatures)
   - MINOR: New features (new commands, flags, integrations)
   - PATCH: Bug fixes, docs, refactoring

### Phase 2: Version & Changelog Updates

6. **Update Cargo.toml**
   - Change `version = "X.Y.Z"` to new version

7. **Update CHANGELOG.md**
   - Add new version section: `## [X.Y.Z] - YYYY-MM-DD`
   - Move relevant entries from `[Unreleased]` or create new entries
   - Update version comparison links at bottom:
     ```markdown
     [Unreleased]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z...HEAD
     [X.Y.Z]: https://github.com/rust-works/omni-dev/compare/vPREV...vX.Y.Z
     ```

### Phase 3: Quality Checks

8. **Run Quality Checks**
   ```bash
   cargo build --release
   cargo test
   cargo clippy -- -D warnings
   ```
   Abort if any check fails.

### Phase 4: Git Operations

9. **Commit Changes**
   ```bash
   git add Cargo.toml Cargo.lock CHANGELOG.md
   git commit -m "$(cat <<'EOF'
   chore: prepare release vX.Y.Z

   - Update version from PREV to X.Y.Z
   - Update CHANGELOG.md with release notes

   🤖 Generated with [Claude Code](https://claude.com/claude-code)

   Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
   EOF
   )"
   ```

10. **Create Annotated Tag**
    ```bash
    git tag -a vX.Y.Z -m "Release version X.Y.Z

    <summary of key changes>
    "
    ```

11. **Push to Remote**
    ```bash
    git push origin main
    git push origin vX.Y.Z
    ```

### Phase 5: Monitor CI Release

12. **Wait for Release Workflow**
    Poll the GitHub Actions release workflow until completion:
    ```bash
    # Get the run ID for the release workflow triggered by the tag
    gh run list --workflow=release.yml --branch=vX.Y.Z --limit=1 --json databaseId,status,conclusion
    ```

13. **Poll Until Complete**
    Loop with 30-second intervals:
    ```bash
    gh run view <run_id> --json status,conclusion
    ```
    - `status: "completed"` + `conclusion: "success"` = Success
    - `status: "completed"` + `conclusion: "failure"` = Failed (show logs)
    - `status: "in_progress"` or `status: "queued"` = Keep polling

14. **On Failure: Show Logs**
    ```bash
    gh run view <run_id> --log-failed
    ```

### Phase 6: Verification

15. **Verify GitHub Release**
    ```bash
    gh release view vX.Y.Z
    ```

16. **Verify crates.io Publication**
    ```bash
    cargo search omni-dev
    ```

17. **Report Success**
    Display:
    - New version number
    - GitHub release URL
    - crates.io URL
    - Changelog summary

## Error Handling

| Error                    | Action                                      |
|--------------------------|---------------------------------------------|
| Dirty working directory  | Abort with message to commit/stash changes  |
| Quality check fails      | Abort with specific failure details         |
| Push fails               | Check remote access and branch protection   |
| CI workflow fails        | Show failed job logs, suggest fixes         |
| Timeout (>15 min)        | Provide manual verification commands        |

## Polling Configuration

- **Initial delay**: 10 seconds (allow workflow to start)
- **Poll interval**: 30 seconds
- **Timeout**: 15 minutes
- **Max polls**: 30

## CI Workflows Triggered

Pushing a `v*` tag triggers:

| Workflow      | Purpose                                           |
|---------------|---------------------------------------------------|
| `ci.yml`      | Tests, linting, Nix build, Cachix publish         |
| `release.yml` | GitHub release, binaries, crates.io publish       |

## Important Notes

- **Do NOT manually run `gh release create`** - CI handles this automatically
- The release workflow creates the GitHub release from the tag
- Cross-platform binaries (Linux, macOS, Windows) are built and attached
- crates.io publication uses `CARGO_REGISTRY_TOKEN` secret

## Commands Reference

```bash
# Check workflow status
gh run list --workflow=release.yml --limit=5

# Watch workflow in real-time
gh run watch <run_id>

# View workflow logs
gh run view <run_id> --log

# View failed job logs only
gh run view <run_id> --log-failed

# Verify release
gh release view vX.Y.Z

# Check crates.io
cargo search omni-dev
```

## Rollback Procedure

If release needs to be rolled back:

1. **Delete GitHub Release** (if created):
   ```bash
   gh release delete vX.Y.Z --yes
   ```

2. **Delete Tags**:
   ```bash
   git tag -d vX.Y.Z
   git push --delete origin vX.Y.Z
   ```

3. **Yank from crates.io** (if published):
   ```bash
   cargo yank --version X.Y.Z
   ```

4. **Revert Commit**:
   ```bash
   git revert HEAD
   git push origin main
   ```
