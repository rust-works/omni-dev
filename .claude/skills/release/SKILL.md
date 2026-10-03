---
name: release
description: Automates the release process for this Rust project and its VS Code extension. Use when creating a new release, preparing a version bump, or publishing to GitHub/crates.io/the extension registries. Triggers on terms like "release", "publish", "version bump", "create tag".
---

# Automated Release Skill

This skill performs the complete end-to-end release process for omni-dev, from version bump to verified publication. [docs/RELEASE.md](../../../docs/RELEASE.md) is the canonical description; keep the two in sync.

## Ground Rules

- **`main` is protected and merges through a merge queue that rebases.** Release preparation goes through a pull request. Never `git push origin main`, never `gh pr merge --admin`.
- **Commit hashes change on merge.** Create tags only **after** the merge, on the rebased commits found by subject on `origin/main`. Never tag a local commit.
- **Two artefacts, two commits, two tags.** The crate (`vX.Y.Z`, `Cargo.toml`, `CHANGELOG.md`) and the VS Code extension (`vscode-vA.B.C`, `editors/vscode/package.json`, `editors/vscode/CHANGELOG.md`) are versioned independently. A release normally cuts both; skip one only if it has nothing new since its last tag.

## Execution Steps

### Phase 1: Preparation

1. **Verify Clean State**
   ```bash
   git status --porcelain
   ```
   Abort if working directory is not clean.

2. **Get Current Versions**
   Read them from `origin/main`, not the local checkout, which may be behind:
   ```bash
   git fetch origin main
   git show origin/main:Cargo.toml | grep -m1 '^version = ' | sed 's/version = "\(.*\)"/\1/'
   git show origin/main:editors/vscode/package.json | jq -r .version
   ```

3. **Get the Last Release Tag per Artefact**
   A bare `git describe --tags` returns whichever tag is nearest, which may be the other artefact's, and `v*` also matches `vscode-v*`:
   ```bash
   git describe --tags --abbrev=0 --match 'v*' --exclude 'vscode-*' origin/main   # crate
   git describe --tags --abbrev=0 --match 'vscode-v*' origin/main                 # extension
   ```

4. **Analyze Changes Since Last Release**
   ```bash
   git log --oneline vPREV..origin/main
   git log --oneline vscode-vPREV..origin/main -- editors/vscode
   ```

5. **Determine Version Bump** (for each artefact that ships)
   - MAJOR: Breaking changes (removed APIs, changed signatures)
   - MINOR: New features (new commands, flags, integrations)
   - PATCH: Bug fixes, docs, refactoring

6. **Cut a Release Branch** from the current `origin/main` (a separate worktree is fine):
   ```bash
   git switch -c release/vX.Y.Z origin/main
   ```

### Phase 2: Documentation & Changelog Review

7. **Check if Docs Need Updates**
   Review documentation for accuracy against new features:
   - `docs/RELEASE.md` - Release process still accurate?
   - `README.md` - Features and examples up to date?
   - `CLAUDE.md` - AI guidance still relevant?
   - `.claude/skills/` - Skills reflect current workflows?

   Update any docs that are outdated before proceeding.

8. **Update CHANGELOG.md** (crate)
   - Reconcile `[Unreleased]` against `git log --oneline vPREV..origin/main`; it is routinely incomplete.
   - Check that nothing landed in the **previous** release's section after its tag was cut (#2129). Any `>` line is a late bullet that belongs in the new section:
     ```bash
     PREV=A.B.C   # previous crate version
     diff <(git show "v$PREV:CHANGELOG.md" | sed -n "/^## \[$PREV\]/,/^## \[/p") \
          <(sed -n "/^## \[$PREV\]/,/^## \[/p" CHANGELOG.md)
     ```
   - Add new version section: `## [X.Y.Z] - YYYY-MM-DD`, and leave an empty `## [Unreleased]` above it
   - Document all changes since last release under appropriate categories:
     - **Added**: New features
     - **Changed**: Changes in existing functionality
     - **Deprecated**: Soon-to-be removed features
     - **Removed**: Now removed features
     - **Fixed**: Bug fixes
     - **Security**: Vulnerability fixes
     - **Documentation**: Docs-only changes
     - **CI/CD**: Build/workflow changes
   - Update version comparison links at bottom:
     ```markdown
     [Unreleased]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z...HEAD
     [X.Y.Z]: https://github.com/rust-works/omni-dev/compare/vPREV...vX.Y.Z
     ```

9. **Update editors/vscode/CHANGELOG.md** (extension)
   - Move `[Unreleased]` into `## [A.B.C] - YYYY-MM-DD`. Both registries render a Changelog tab from it, so every published version needs an entry.
   - Reconcile against `git log --oneline vscode-vPREV..origin/main -- editors/vscode`. This file lags more often than the root one: changes are often recorded only in the root changelog.

### Phase 3: Version Update

10. **Update Cargo.toml**
    - Change `version = "X.Y.Z"` to new version (building in Phase 4 refreshes `Cargo.lock`)

11. **Update the Extension Version** (bumps `package.json` and the two omni-dev entries in `package-lock.json`, not dependency ranges)
    ```bash
    (cd editors/vscode && npm version A.B.C --no-git-tag-version --ignore-scripts --allow-same-version)
    git diff --stat -- editors/vscode   # expect only package.json and package-lock.json, at most 3 changed lines
    ```
    Use a subshell so later steps run from the repository root. `--allow-same-version` covers a `package.json` a feature commit already bumped: without it npm stops at `Version not changed` and the stale lockfile entries are never repaired.

### Phase 4: Quality Checks

12. **Run Crate Quality Checks**
    ```bash
    cargo build --release
    cargo test
    cargo clippy -- -D warnings
    cargo fmt --all -- --check
    ```
    Abort if any check fails.

13. **Run Extension Checks**
    ```bash
    (cd editors/vscode && npm ci && npm run typecheck && npm run build && npm test && npm run package)
    ```
    Abort if any check fails.

14. **Check the Extension's Publish Tokens**
    The Marketplace and Open VSX are published independently, so an expired token fails the run without keeping the release from the other registry (`vscode-v0.9.0`, `v0.10.0` and `v0.11.0` all failed at the Marketplace step with an expired `VSCE_PAT`). If a token is available in the environment, verify it; never print it and never read credential files to find one:
    ```bash
    VSCE_PAT=<token> npx @vscode/vsce verify-pat rust-works
    OVSX_PAT=<token> npx ovsx verify-pat rust-works
    ```
    `gh secret list` shows only when a secret was set, not whether it works. If a token cannot be verified or fails, tell the user before tagging the extension and carry on with the crate if they agree.

### Phase 5: Commit, Pull Request and Merge Queue

15. **Commit the Crate Release**
    ```bash
    git add Cargo.toml Cargo.lock CHANGELOG.md
    git commit -m "$(cat <<'EOF'
    chore(release): prepare release vX.Y.Z

    - Update version from PREV to X.Y.Z
    - Update CHANGELOG.md with release notes
    EOF
    )"
    ```
    The subject must keep this exact form: Phase 6 finds the rebased commit by it.

16. **Commit the Extension Release**
    ```bash
    git add editors/vscode/package.json editors/vscode/package-lock.json editors/vscode/CHANGELOG.md
    git commit -m "$(cat <<'EOF'
    chore(release): prepare vscode extension release vA.B.C

    - Update version from PREV to A.B.C in package.json and package-lock.json
    - Update the extension CHANGELOG.md with release notes
    EOF
    )"
    ```

17. **Open the Pull Request and Enqueue It**
    ```bash
    omni-dev git commit message lint origin/main..HEAD
    git push -u origin release/vX.Y.Z
    gh pr create --title "chore(release): prepare release vX.Y.Z and vscode extension vA.B.C" --body "Release preparation."
    gh pr checks --watch
    gh pr merge <PR>        # enqueues; never --admin
    ```

18. **Wait for the Merge**
    ```bash
    gh pr view <PR> --json state,mergeCommit
    ```
    Poll until `state` is `MERGED`. If `main` moved and `CHANGELOG.md` now conflicts, rebase the branch onto `origin/main`, resolve it, push and enqueue again. If the queue ejects the entry, fix the failing check on the branch and re-enqueue.

### Phase 6: Tag the Rebased Commits

19. **Locate the Commits as They Landed on `main`**
    Phases 6 and 7 each have a crate half and an extension half; do only the halves for the artefacts being released. The dots are escaped because `--grep` takes a regular expression:
    ```bash
    git fetch origin main
    CRATE=$(git log origin/main --format=%H -1 --grep='^chore(release): prepare release vX\.Y\.Z$')
    EXT=$(git log origin/main --format=%H -1 --grep='^chore(release): prepare vscode extension release vA\.B\.C$')
    ```
    Abort if a lookup you need is empty. If a fix had to land after the release commit, tag the fix's hash instead: it carries the correct files, and the subject lookup would return the original commit.

20. **Create Annotated Tags on Those Commits** (not on `HEAD`)
    ```bash
    git tag -a vX.Y.Z -m "Release version X.Y.Z

    <summary of key changes>
    " "$CRATE"

    git tag -a vscode-vA.B.C -m "VS Code extension A.B.C

    <summary of key changes>
    " "$EXT"
    ```

21. **Verify the Tagged Commits Before Pushing**
    `release.yml` does not compare the tag with `Cargo.toml`, so verify locally while a bad tag can still be deleted:
    ```bash
    # On main
    git merge-base --is-ancestor vX.Y.Z origin/main && echo ok
    git merge-base --is-ancestor vscode-vA.B.C origin/main && echo ok

    # Versions at the tag: all must read the new version
    git show vX.Y.Z:Cargo.toml | grep -m1 '^version = '
    git show vX.Y.Z:Cargo.lock | grep -A1 '^name = "omni-dev"$'
    git show vscode-vA.B.C:editors/vscode/package.json | jq -r .version
    git show vscode-vA.B.C:editors/vscode/package-lock.json | jq -r '.version, .packages[""].version'

    # Nothing left under [Unreleased] at either tag: both must print nothing
    git show vX.Y.Z:CHANGELOG.md | awk '/^## \[Unreleased\]/{f=1;next} /^## \[/{f=0} f && NF'
    git show vscode-vA.B.C:editors/vscode/CHANGELOG.md | awk '/^## \[Unreleased\]/{f=1;next} /^## \[/{f=0} f && NF'
    ```
    On any failure, `git tag -d <tag>`, fix it in a follow-up pull request and start again from Phase 5. Do not push a bad tag.

22. **Push the Tags**
    ```bash
    git push origin vX.Y.Z vscode-vA.B.C   # only the tags you created
    ```

### Phase 7: Monitor CI Release

23. **Wait for the Release Workflows**
    Poll the GitHub Actions workflows until completion:
    ```bash
    # Get the run ID for the workflow triggered by each tag
    gh run list --workflow=release.yml --branch=vX.Y.Z --limit=1 --json databaseId,status,conclusion
    gh run list --workflow=vscode-extension-release.yml --branch=vscode-vA.B.C --limit=1 --json databaseId,status,conclusion
    ```

24. **Poll Until Complete**
    Loop with 30-second intervals:
    ```bash
    gh run view <run_id> --json status,conclusion
    ```
    - `status: "completed"` + `conclusion: "success"` = Success
    - `status: "completed"` + `conclusion: "failure"` = Failed (show logs)
    - `status: "in_progress"` or `status: "queued"` = Keep polling

25. **On Failure: Show Logs**
    ```bash
    gh run view <run_id> --log-failed
    ```
    For an extension run that failed at a publish step (typically an expired `VSCE_PAT`), the other registry was still attempted. Tell the user to renew the secret, then re-run on the same tag; `--skip-duplicate` leaves a registry that already has the version alone:
    ```bash
    gh run rerun <run_id> --failed
    ```

### Phase 8: Verification

26. **Verify GitHub Release**
    ```bash
    gh release view vX.Y.Z --json assets --jq '.assets[].name'
    ```
    Expect `omni-dev-linux.tar.gz`, `omni-dev-linux-arm64.tar.gz`, `omni-dev-macos-arm64.tar.gz` and `omni-dev-windows.zip`. A missing asset means a matrix leg failed, and `publish-crates` is blocked with it. After a transient failure (runner outage, network) `gh run rerun <run_id> --failed` recovers it, after `gh release delete-asset vX.Y.Z <asset-name>` if the upload then reports the asset already exists. A defect in the code or the workflow needs a patch release, because a re-run uses the tag's commit. Do not continue without the asset.

27. **Verify crates.io Publication**
    ```bash
    cargo search omni-dev --limit 1
    ```

28. **Verify the Released Binary Reports the New Version**
    ```bash
    d=$(mktemp -d)
    gh release download vX.Y.Z --pattern 'omni-dev-macos-arm64.tar.gz' --dir "$d"   # pick the host platform's asset
    tar xzf "$d/omni-dev-macos-arm64.tar.gz" -C "$d"
    "$d/omni-dev" --version                        # omni-dev X.Y.Z (<short sha> <date>)
    git rev-parse 'vX.Y.Z^{commit}'                # full SHA; it must start with <short sha>
    ```
    The short SHA's length varies with the machine that built the binary, so compare by prefix.
    Only the host's own asset can be run. Extract each of the others the same way and run `file "$d/omni-dev"` to confirm the architecture its name claims, notably `omni-dev-linux-arm64.tar.gz` (`ELF 64-bit ... ARM aarch64`) against `omni-dev-linux.tar.gz` (`x86-64`).

29. **Verify the Extension**
    The workflow's publish steps are the evidence: both `Publish to VS Code Marketplace` and `Publish to Open VSX` must be green, not skipped. Open VSX can be read back; the Marketplace cannot be relied on, since read APIs lag a publish and `vsce show` can answer `not found` for a live extension:
    ```bash
    curl -s https://open-vsx.org/api/rust-works/omni-dev/latest | jq -r .version
    ```

30. **Report Success**
    Display:
    - New crate and extension versions
    - GitHub release URL
    - crates.io URL
    - Registry status for the extension
    - Changelog summary

### Phase 9: Manual Glama Listing Update

31. **Prompt the User to Update Glama**
    The Glama MCP listing pins its Docker build to a specific commit SHA and only rebuilds when that SHA is bumped. This step is a web-UI action that cannot be automated from here.

    Display to the user:
    - The short release SHA: `git rev-parse --short 'vX.Y.Z^{commit}'` (the `^{commit}` matters: the tag is annotated, so the bare tag name returns the tag object's hash, not a commit)
    - The admin URL: <https://glama.ai/mcp/servers/rust-works/omni-dev/admin/dockerfile>
    - A pointer to the procedure: `docs/glama-listing.md`

    Do not block on this — it's an out-of-band manual step the human performs after the automated release lands.

## Error Handling

| Error                          | Action                                                                   |
|--------------------------------|--------------------------------------------------------------------------|
| Dirty working directory        | Abort with message to commit/stash changes                               |
| Quality check fails            | Abort with specific failure details                                      |
| Direct push to `main` rejected | Expected: `main` is protected. Use the pull request and merge queue      |
| Queue ejects the entry         | Show the failing check, fix on the release branch, re-enqueue            |
| Tag verification fails         | Delete the local tag, fix via a follow-up PR; never push a bad tag       |
| CI workflow fails              | Show failed job logs, suggest fixes                                      |
| Registry publish fails         | Other registry still attempted; renew the token, `gh run rerun --failed` |
| Timeout (>15 min)              | Provide manual verification commands                                     |

## Polling Configuration

- **Initial delay**: 10 seconds (allow workflow to start)
- **Poll interval**: 30 seconds
- **Timeout**: 15 minutes
- **Max polls**: 30

## CI Workflows Triggered

Pushing a `v*` tag triggers (the `!vscode-*` exclusion keeps the extension's tags out of both):

| Workflow      | Purpose                                                            |
|---------------|--------------------------------------------------------------------|
| `ci.yml`      | Tests, linting, Windows build (Coverage is skipped on tags, #1289) |
| `release.yml` | GitHub release, binaries, crates.io publish                        |

Pushing a `vscode-v*` tag triggers:

| Workflow                       | Purpose                                                    |
|--------------------------------|------------------------------------------------------------|
| `vscode-extension-release.yml` | Verify tag, build/test/package, publish to both registries |

## Important Notes

- **Do NOT manually run `gh release create`** - CI handles this automatically
- The release workflow creates the GitHub release from the tag
- Cross-platform binaries (Linux, macOS, Windows) are built and attached
- crates.io publication uses `CARGO_REGISTRY_TOKEN` secret
- Extension publication uses the `VSCE_PAT` and `OVSX_PAT` secrets; each registry is skipped if its token is unset
- The coverage action resolves `version: latest`, so a change that needs a new omni-dev flag only passes its Coverage check once that flag is in a **released** binary: releasing is what unblocks it

## Commands Reference

```bash
# Check workflow status
gh run list --workflow=release.yml --limit=5
gh run list --workflow=vscode-extension-release.yml --limit=5

# Watch workflow in real-time
gh run watch <run_id>

# View workflow logs
gh run view <run_id> --log

# View failed job logs only
gh run view <run_id> --log-failed

# Re-run only the failed jobs on the same tag
gh run rerun <run_id> --failed

# Verify release
gh release view vX.Y.Z

# Check crates.io
cargo search omni-dev --limit 1
```

## Failed Release Recovery

If the release has already been published, keep its tag and commit as a record of the published version. Correct the problem on `main` through a pull request, then prepare a new patch release following `docs/RELEASE.md`. Do not blindly revert the branch tip: another commit may have landed since, and a direct push to `main` is rejected. A registry will not take the same version twice, so the extension is fixed forward the same way.

If the crate was published and must no longer be selected by Cargo, yank that version:

```bash
cargo yank --version X.Y.Z
```

If the GitHub release itself must go:

```bash
gh release delete vX.Y.Z --yes
```

Delete a tag only when its workflows failed before anything reached a registry, and then only to push the corrected one:

```bash
git tag -d vX.Y.Z
git push --delete origin vX.Y.Z
```
