# Release Process

This document outlines the release process for omni-dev: preparing the version and changelog commits, landing them through the merge queue, tagging, and verifying the automated release workflows.

## Overview

This repository ships two independently versioned artefacts:

| Artefact               | Version lives in                                      | Changelog                     | Tag             | Workflow                       | Published to                         |
|------------------------|-------------------------------------------------------|-------------------------------|-----------------|--------------------------------|--------------------------------------|
| **Crate** (`omni-dev`) | `Cargo.toml` and `Cargo.lock`                         | `CHANGELOG.md`                | `vX.Y.Z`        | `release.yml`                  | GitHub release (binaries), crates.io |
| **VS Code extension**  | `editors/vscode/package.json` and `package-lock.json` | `editors/vscode/CHANGELOG.md` | `vscode-vA.B.C` | `vscode-extension-release.yml` | VS Code Marketplace, Open VSX        |

A release normally cuts both. The versions are unrelated, so an artefact with nothing new since its last tag can be skipped; every step below says which artefact it applies to.

`main` is protected and merges through a **merge queue that rebases**. Two consequences shape the whole process:

- **Release preparation goes through a pull request.** `git push origin main` is rejected.
- **The commits you write are not the commits that land.** The queue rebases them, so their hashes change. Tags must therefore be created **after** the merge, on the rebased commits on `main`, never on the local ones.

### Manual Steps (You Do)
1. Decide what ships and cut a release branch
2. Prepare the crate release (version, changelog, quality checks) in one commit
3. Prepare the extension release (version, changelog) in a second commit
4. Check the extension's publish tokens
5. Open one pull request and enqueue it
6. After it merges, create the annotated tags on the rebased commits
7. Verify the tagged commits, then push the tags
8. Monitor and verify the automated releases
9. Update the Glama listing (see [Glama Listing Update](#glama-listing-update))

### Automated Steps (CI Does)
- GitHub release creation and cross-platform binary builds (Linux, macOS, Windows)
- crates.io publication
- Extension build, test and publication to both registries

## Prerequisites

Before starting a release, ensure you have:
- [ ] Permission to open a pull request and add it to the merge queue (never `--admin`)
- [ ] All tests passing locally
- [ ] A clean working directory
- [ ] GitHub repo description and topics still accurately describe the project
  - Check: `gh repo view rust-works/omni-dev --json description,repositoryTopics`
  - Update via `gh repo edit` (description) or `gh api -X PUT repos/rust-works/omni-dev/topics` (topics) if the project's scope has shifted since the last release. See [issue #831](https://github.com/rust-works/omni-dev/issues/831) for the original baseline.
- [ ] (Extension) Both publish tokens are valid — see [Check the Publish Tokens](#4-check-the-publish-tokens)

## Release Steps

### 1. Decide What Ships and Cut a Release Branch

Find the last tag **per artefact**. A bare `git describe --tags` returns whichever tag is nearest, which may be the other artefact's:

```bash
git fetch origin main

# Crate: `v*` also matches `vscode-v*`, so exclude it
git describe --tags --abbrev=0 --match 'v*' --exclude 'vscode-*' origin/main

# Extension
git describe --tags --abbrev=0 --match 'vscode-v*' origin/main
```

List what changed since each tag:

```bash
git log --oneline vPREV..origin/main
git log --oneline vscode-vPREV..origin/main -- editors/vscode
```

Choose the next version for each artefact following [Semantic Versioning](https://semver.org/):
- **MAJOR** (X): Breaking changes
- **MINOR** (Y): New features (backward compatible)
- **PATCH** (Z): Bug fixes (backward compatible)

Cut a release branch from the current `origin/main`:

```bash
git switch -c release/vX.Y.Z origin/main
```

A separate worktree works equally well; what matters is that the branch starts from `origin/main`.

### 2. Prepare the Crate Release

**Version.** Update `Cargo.toml`:

```toml
[package]
version = "X.Y.Z"
```

The quality checks below build the crate, which rewrites the `omni-dev` entry in `Cargo.lock`; commit it with the version.

**Changelog.** Assemble `CHANGELOG.md` from the fragments in [`changelog.d/`](../changelog.d/README.md) ([#2213](https://github.com/rust-works/omni-dev/issues/2213)). Pull requests do not edit the changelog; each adds a `changelog.d/<issue>.<type>.md` file, and `collect` renders them into the release section in [Keep a Changelog](https://keepachangelog.com/) order (Added, Changed, Deprecated, Removed, Fixed, Security, then CI/CD and Documentation).

1. Reconcile the fragments against the commit log first. The `Changelog` check requires a fragment or a `Changelog: none` trailer on every pull request, but a trailer can be wrong, so compare `git log --oneline vPREV..origin/main` with `ls changelog.d/` and add a fragment for anything user-visible that is missing.

2. Preview, then write the section:

   ```bash
   python3 scripts/changelog.py collect --component crate --version X.Y.Z --dry-run
   python3 scripts/changelog.py collect --component crate --version X.Y.Z
   ```

   `collect` adds `## [X.Y.Z] - <today>` under an empty `## [Unreleased]`, sorts entries by issue number, updates the `[Unreleased]` and `[X.Y.Z]` compare links at the bottom, and deletes the fragments it consumed (`--date YYYY-MM-DD` overrides the date). The first release after #2213 also carries the text written by hand under `[Unreleased]` before fragments existed into the section verbatim, after the fragment sections.

3. Read the rendered section and edit it in place if an entry needs it: the release commit is the one place the changelog is edited by hand.

**Quality checks.** Run them to ensure the release is ready:

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo build --release
```

**Commit.** One commit for the crate, with the conventional-commit scope `release`:

```bash
git add -A -- Cargo.toml Cargo.lock CHANGELOG.md changelog.d ':!changelog.d/vscode'
git commit -m "chore(release): prepare release vX.Y.Z

- Update version from PREV to X.Y.Z
- Assemble CHANGELOG.md from the changelog.d fragments"
```

The subject must match this form exactly: step 6 finds the rebased commit by it.

### 3. Prepare the Extension Release

The extension's version and notes are independent of the crate's. See [`editors/vscode/README.md`](../editors/vscode/README.md#releasing) for the one-time registry account setup.

**Version.** From `editors/vscode`, bump `package.json` and the two omni-dev entries at the top of `package-lock.json` without touching dependency ranges:

```bash
(cd editors/vscode && npm version A.B.C --no-git-tag-version --ignore-scripts --allow-same-version)
git diff --stat -- editors/vscode   # expect only package.json and package-lock.json, at most 3 changed lines
```

The subshell keeps your shell at the repository root for the `git add` below. Prefer this to a plain `npm install`, which re-resolves the whole lockfile and can pull unrelated dependency changes into the release commit. `--allow-same-version` covers a feature commit that already bumped `package.json` and left `package-lock.json` behind (this has happened): without it npm stops at `Version not changed` and never repairs the lockfile. Either way, check that all three version strings agree.

**Changelog.** Assemble [`editors/vscode/CHANGELOG.md`](../editors/vscode/CHANGELOG.md) from the fragments in `changelog.d/vscode/`. Both registries render a **Changelog** tab from it, so every published version needs an entry. Reconcile the fragments against the log first:

```bash
git log --oneline vscode-vPREV..origin/main -- editors/vscode
ls changelog.d/vscode/
python3 scripts/changelog.py collect --component vscode --version A.B.C --dry-run
python3 scripts/changelog.py collect --component vscode --version A.B.C
```

The extension's changelog has no compare links, so `collect` only adds the section and deletes the consumed fragments. It refuses a release with no fragments and an empty `[Unreleased]`; write a fragment saying what the release carries rather than publish a version with no entry.

**Checks.** The release workflow re-runs these, so run them first:

```bash
(cd editors/vscode && npm ci && npm run typecheck && npm run build && npm test && npm run package)
```

**Commit.**

```bash
git add editors/vscode/package.json editors/vscode/package-lock.json editors/vscode/CHANGELOG.md && git add -A -- changelog.d/vscode
git commit -m "chore(release): prepare vscode extension release vA.B.C

- Update version from PREV to A.B.C in package.json and package-lock.json
- Assemble the extension CHANGELOG.md from the changelog.d/vscode fragments"
```

### 4. Check the Publish Tokens

`vscode-extension-release.yml` publishes to the VS Code Marketplace and to Open VSX **independently**: a failure at one does not skip the other, but the run still ends red. Both tokens expire. `vscode-v0.9.0`, `v0.10.0` and `v0.11.0` all failed at the Marketplace step with an expired `VSCE_PAT`; those runs predate the independent publish, so each also skipped Open VSX, which is why Open VSX is still at 0.8.0.

Verify the tokens you hold before tagging:

```bash
VSCE_PAT=<token> npx @vscode/vsce verify-pat rust-works
OVSX_PAT=<token> npx ovsx verify-pat rust-works
```

`gh secret list` shows when a repository secret was last **set**, never whether it still works, and a secret cannot be read back. If you rotate a token, update the repository secret too and confirm its timestamp moved. A passing local check only proves the token you hold.

### 5. Open the Pull Request and Enqueue It

One pull request can carry both commits, as in [#2123](https://github.com/rust-works/omni-dev/pull/2123):

```bash
git push -u origin release/vX.Y.Z
gh pr create --title "chore(release): prepare release vX.Y.Z and vscode extension vA.B.C" \
  --body "Release preparation."
gh pr checks --watch
gh pr merge <PR>        # enqueues; never --admin
```

Check the commit messages locally before pushing with `omni-dev git commit message lint origin/main..HEAD`.

The merge queue builds the entry on a `gh-readonly-queue/main/...` branch and rebases it onto `main`. Wait until the pull request reports `MERGED`:

```bash
gh pr view <PR> --json state,mergeCommit
```

If `main` moves before the queue reaches your entry and the branch now conflicts (a version file, or a changelog another release touched), rebase the branch onto `origin/main`, resolve it, push and enqueue again. Feature pull requests add their own fragment files rather than editing `CHANGELOG.md`, so they no longer conflict there.

### 6. Create the Tags on the Rebased Commits

Steps 6 and 7 each have a crate half and an extension half. Do only the halves for the artefacts you are releasing.

Only now, find the commits **as they landed on `main`**. Look them up by subject, not by the hash you committed locally (the dots in the version are escaped because `--grep` takes a regular expression):

```bash
git fetch origin main
CRATE=$(git log origin/main --format=%H -1 --grep='^chore(release): prepare release vX\.Y\.Z$')
EXT=$(git log origin/main --format=%H -1 --grep='^chore(release): prepare vscode extension release vA\.B\.C$')
echo "crate=$CRATE extension=$EXT"   # each one you are releasing must be non-empty
```

If a fix had to land after the release commit, tag the fix's hash instead: it is the commit that carries the correct files, and the subject lookup would return the original one.

Create annotated tags on those commits, not on `HEAD` (more commits may have landed on `main` since):

```bash
git tag -a vX.Y.Z -m "Release version X.Y.Z

<summary of the key changes>" "$CRATE"

git tag -a vscode-vA.B.C -m "VS Code extension A.B.C

<summary of the key changes>" "$EXT"
```

### 7. Verify the Tagged Commits, Then Push the Tags

`release.yml` does **not** compare the tag with `Cargo.toml`, so a wrong version would be built and published as-is. `vscode-extension-release.yml` fails on a tag/`package.json` mismatch, but only once it is already running. Verify locally, before the push, while a bad tag can still just be deleted.

```bash
# The tagged commit is on main
git merge-base --is-ancestor vX.Y.Z origin/main && echo ok
git merge-base --is-ancestor vscode-vA.B.C origin/main && echo ok

# Crate: version in Cargo.toml and in Cargo.lock at the tag — both must read X.Y.Z
git show vX.Y.Z:Cargo.toml | grep -m1 '^version = '
git show vX.Y.Z:Cargo.lock | grep -A1 '^name = "omni-dev"$'

# Extension: package.json and the two lockfile entries — all three must read A.B.C
git show vscode-vA.B.C:editors/vscode/package.json | jq -r .version
git show vscode-vA.B.C:editors/vscode/package-lock.json | jq -r '.version, .packages[""].version'

# Nothing left under [Unreleased] at either tag — both commands must print nothing
git show vX.Y.Z:CHANGELOG.md | awk '/^## \[Unreleased\]/{f=1;next} /^## \[/{f=0} f && NF'
git show vscode-vA.B.C:editors/vscode/CHANGELOG.md | awk '/^## \[Unreleased\]/{f=1;next} /^## \[/{f=0} f && NF'
```

If anything is wrong, delete the local tag (`git tag -d <tag>`) and fix it with a follow-up pull request. Do not push a bad tag.

Then push the tags (this triggers all automated release steps):

```bash
git push origin vX.Y.Z vscode-vA.B.C   # only the tags you created
```

## Automated Release Pipeline

The two tag families trigger separate workflows and never each other's: `release.yml` and `ci.yml` exclude `vscode-*`, and `vscode-extension-release.yml` runs only on `vscode-v*` (or a manual dispatch), never on a push to `main`.

### Crate: pushing `vX.Y.Z`

**CI Workflow (`.github/workflows/ci.yml`)**
- Runs the full suite: tests on stable, beta and nightly Rust, formatting, clippy, documentation, a Windows build, the security audit, the dependency policy and secret scanning
- **Skips Coverage on tags** ([#1289](https://github.com/rust-works/omni-dev/issues/1289)): coverage is checked on PRs and main pushes; the patchcov action installs patchcov independently of omni-dev releases

**Release Workflow (`.github/workflows/release.yml`)**
- **Creates GitHub Release**: Automatically from the tag
- **Builds Cross-Platform Binaries** (both `omni-dev` and `omni-dev-mcp` for each target):
  - Linux (x86_64-unknown-linux-gnu), as `omni-dev-linux.tar.gz`, built on `ubuntu-22.04`
  - Linux ARM64 (aarch64-unknown-linux-gnu), as `omni-dev-linux-arm64.tar.gz`, built natively on GitHub's `ubuntu-22.04-arm` runner ([#2116](https://github.com/rust-works/omni-dev/issues/2116))
  - Both Linux legs then run `scripts/check_glibc_floor.py` and fail if a binary needs a newer glibc than the floor ([below](#linux-glibc-floor))
  - macOS (aarch64-apple-darwin)
  - Windows (x86_64-pc-windows-msvc)
- **Uploads Release Assets**: Attaches compiled binaries to the GitHub release
- **Publishes to crates.io**: Automatically using the `CARGO_REGISTRY_TOKEN` secret

### Extension: pushing `vscode-vA.B.C`

**Extension Release Workflow (`.github/workflows/vscode-extension-release.yml`)**
- Verifies the tag matches `editors/vscode/package.json`
- Re-runs typecheck, build, tests and packaging
- Publishes the **same** `.vsix` to the VS Code Marketplace (`VSCE_PAT`) and to Open VSX (`OVSX_PAT`), independently: a failure publishing to one does not skip the other. Both publishes pass `--skip-duplicate`, so a re-run leaves a registry that already has the version alone
- A registry whose token is unset is skipped with a notice. The run fails if neither is set, or if either publish fails
- Uploads the `.vsix` as a workflow artefact for provenance

### 8. Monitor and Verify

After pushing the tags, monitor the automated releases.

### Crate

1. **Check GitHub Actions**: Watch the release workflow progress
   ```bash
   gh run list --workflow=release.yml
   gh run watch
   ```

2. **Verify the GitHub Release** and its four assets:
   ```bash
   gh release view vX.Y.Z --json assets --jq '.assets[].name'
   # omni-dev-linux.tar.gz, omni-dev-linux-arm64.tar.gz, omni-dev-macos-arm64.tar.gz, omni-dev-windows.zip
   ```
   A leg that fails to build leaves its asset out of the release, and because `publish-crates` waits on every leg it also holds back the crates.io publish. After a transient failure (a runner outage, a network error) re-run the failed jobs with `gh run rerun <run_id> --failed`; if the upload then fails because the asset already exists, delete the stale one first with `gh release delete-asset vX.Y.Z <asset-name>`. A defect in the code or the workflow is not fixed by a re-run, which uses the tag's commit: it needs a patch release. Do not carry on without the asset, since consumers download these by exact name.

3. **Verify crates.io Publication**:
   ```bash
   cargo search omni-dev --limit 1
   ```

4. **Download the released binary and confirm it reports the new version**, and the commit you tagged (the short SHA's length varies with the machine that built it, so compare by prefix):
   ```bash
   d=$(mktemp -d)
   gh release download vX.Y.Z --pattern 'omni-dev-macos-arm64.tar.gz' --dir "$d"   # pick your platform's asset
   tar xzf "$d/omni-dev-macos-arm64.tar.gz" -C "$d"
   "$d/omni-dev" --version                        # omni-dev X.Y.Z (<short sha> <date>)
   git rev-parse 'vX.Y.Z^{commit}'                # full SHA; it must start with <short sha>
   ```
   Only the host's own asset can be run. Confirm that each of the others, notably `omni-dev-linux-arm64.tar.gz`, holds the architecture its name claims by extracting it the same way and running `file "$d/omni-dev"` (`ELF 64-bit ... ARM aarch64` for the ARM64 Linux asset, `ELF 64-bit ... x86-64` for `omni-dev-linux.tar.gz`).

### Extension

1. **Check the workflow run.** Its publish steps are the evidence:
   ```bash
   gh run list --workflow=vscode-extension-release.yml --limit 3
   gh run view <run_id>
   ```
   Both `Publish to VS Code Marketplace` and `Publish to Open VSX` must be green, not skipped.

2. **Read Open VSX back.** Its API is reliable:
   ```bash
   curl -s https://open-vsx.org/api/rust-works/omni-dev/latest | jq -r .version
   ```

3. **Do not rely on a Marketplace read-back.** Registry read APIs lag a publish, and `vsce show rust-works.omni-dev` can answer `not found` for an extension that is live. Trust the publish step's output, and check the [Marketplace page](https://marketplace.visualstudio.com/items?itemName=rust-works.omni-dev) later by eye.

4. **If a publish step failed**, the run is red but the other registry was still attempted. Fix the secret ([step 4](#4-check-the-publish-tokens)), then re-run the failed job on the **same tag**; `--skip-duplicate` leaves a registry that already has the version alone, and the version is still free on the one that failed. A re-run uses the workflow file as of the tag's commit:
   ```bash
   gh run rerun <run_id> --failed
   ```

## Linux glibc floor

The Linux release binaries link dynamically against the glibc of the image that builds them, so on a host with an older glibc `omni-dev --version` dies in the dynamic loader before omni-dev's own code runs ([#2178](https://github.com/rust-works/omni-dev/issues/2178)). The floor is **glibc 2.35** (Ubuntu 22.04), and it is deliberate and checked, not a side effect of the runner:

- **The image is pinned.** The Linux legs of `release.yml` build on `ubuntu-22.04` and `ubuntu-22.04-arm`, never on a moving label such as `ubuntu-latest`, which would raise the floor when GitHub moves it to the next Ubuntu.
- **The floor is checked.** After the build, `scripts/check_glibc_floor.py --floor "$GLIBC_FLOOR"` reads each binary's version-needs table (`readelf -V`) and fails the leg if the highest non-weak `GLIBC_x.y` is above `GLIBC_FLOOR` (set once, in `release.yml`'s `env`). A weak requirement (the loader's non-fatal `weak version` line) is reported but does not fail. It also fails, rather than passing, when it finds no `GLIBC_` requirement at all, since that means the output was not what it parses; its exit status is 1 for a binary above the floor and 2 when `readelf` is missing or cannot read the binary. `publish-crates` waits on every leg, so a failure also holds back the crates.io publish; fix it and release a patch, as for any workflow defect (see [Monitor and Verify](#8-monitor-and-verify)). The check runs only on a tag push, so a runner-image change is first seen on the release it affects; run the script by hand against a locally built `target/release/omni-dev` on Linux to see where a change would land.
- **Changing it.** Raising the floor is a decision to drop hosts: edit `GLIBC_FLOOR` and the runner image together, and update the floor stated in the [README](../README.md#installation) and here. GitHub retires old images eventually; when `ubuntu-22.04` goes, the build fails visibly and the options are a newer floor, building with `cargo zigbuild --target <triple>.2.17`, or a static `musl` build (neither has been tried; the check works unchanged with either).

To see what a binary needs: `readelf -V omni-dev` and read `.gnu.version_r`, or `python3 scripts/check_glibc_floor.py --floor 2.35 omni-dev`.

## Ordering with Dependents

The `action-works/patchcov-action@v1.0` action used by `ci.yml` installs patchcov 0.1.1 independently of omni-dev. Coverage functionality now lives in [rust-works/patchcov](https://github.com/rust-works/patchcov) (#2200). Changes that need new patchcov behavior require a published patchcov version and an action version input update; releasing omni-dev does not unblock them.

## Post-Release Tasks

After the automated release completes:

1. **Update Documentation** (if needed):
   - Update any version-specific documentation
   - Ensure README examples use current version

2. **Update the Glama Listing** (see [Glama Listing Update](#glama-listing-update))

3. **Announce Release** (optional):
   - Share release notes with team
   - Update project status if needed

## Glama Listing Update

After the GitHub release lands, point the [Glama listing](https://glama.ai/mcp/servers/rust-works/omni-dev) at the new commit so the public listing reflects the release. This is a manual web-UI step — Glama's Docker build is pinned to a specific commit SHA and only rebuilds when that SHA is bumped.

See [Glama Listing](glama-listing.md) for the full procedure, the canonical admin-form values (Build steps, CMD arguments, env-var schema), and troubleshooting. The short version:

1. Run `git rev-parse --short 'vX.Y.Z^{commit}'` to get the release SHA. The `^{commit}` matters: the tag is annotated, so `git rev-parse vX.Y.Z` alone returns the tag object's hash, which is not a commit Glama can build.
2. Paste it into **Pinned commit SHA** at <https://glama.ai/mcp/servers/rust-works/omni-dev/admin/dockerfile> and **Save**.
3. Trigger a **build**, wait for it to go green, then trigger a **release**.

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
- Ensure the crates.io token is configured
- Check for naming conflicts
- Verify all dependencies are published
- For the extension, see [Check the Publish Tokens](#4-check-the-publish-tokens) and the re-run instructions under [Monitor and Verify](#8-monitor-and-verify)

**Merge Queue Entry Ejected**:
- The queue waits for every required context on its `gh-readonly-queue/main/...` branch; open the failing check from the pull request, fix it on the release branch and re-enqueue
- Never bypass the queue with `--admin`

**Tag Conflicts** (a tag you pushed in error, before its release finished):
```bash
# Delete local tag
git tag -d vX.Y.Z

# Delete remote tag
git push --delete origin vX.Y.Z
```
Pushing the corrected tag again re-triggers the workflows. Only do this when nothing reached a registry.

### Recovery

If a release has a problem, **fix forward**:

1. **Published versions are kept.** Keep the tag and its commit as the record of what was published. A registry will not take the same version twice, so correct the problem on `main` in a new commit and prepare a **patch release** with this document. Do not blindly revert the branch tip: other commits may have landed since, and a direct `git push origin main` is rejected anyway. A revert is a pull request through the queue.
2. **GitHub Release**: Mark as pre-release or delete (`gh release delete vX.Y.Z --yes`)
3. **crates.io**: Cannot delete, but can yank so Cargo stops selecting it: `cargo yank --version X.Y.Z`
4. **Extension**: publish a patch version; do not reuse the number

## CI/CD Configuration

The automated release pipeline requires these GitHub secrets:

| Secret                 | Purpose                                                       |
|------------------------|---------------------------------------------------------------|
| `GITHUB_TOKEN`         | Automatically provided by GitHub Actions for release creation |
| `CARGO_REGISTRY_TOKEN` | crates.io API token for publishing                            |
| `VSCE_PAT`             | VS Code Marketplace token for the `rust-works` publisher      |
| `OVSX_PAT`             | Open VSX token for the `rust-works` namespace                 |

### Workflow Files

- `.github/workflows/ci.yml` - Quality checks (also runs on merge-queue entries)
- `.github/workflows/release.yml` - Release creation and publishing
- `.github/workflows/vscode-extension.yml` - Extension checks on pull requests and `main`
- `.github/workflows/vscode-extension-release.yml` - Extension publication (`vscode-v*` tags only)
- `.github/workflows/commit-lint.yml` - PR commit message validation
- `.github/workflows/changelog.yml` - Changelog Check on pull requests and merge-queue entries ([below](#changelog-check))

### Changelog Check

`scripts/changelog.py` ([#2213](https://github.com/rust-works/omni-dev/issues/2213), [ADR-0097](adrs/adr-0097.md)) replaced `scripts/check_changelog.py` (#2129). Pull requests add [changelog fragments](../changelog.d/README.md) instead of editing either changelog, so two in flight never conflict, and none can have its bullet applied inside a section a release has since closed. The workflow runs:

- **`check`**, on pull requests and merge-queue entries: every file in `changelog.d/` and `changelog.d/vscode/` other than `README.md` must be a valid fragment (`<id>.<type>[.<n>].md`, a known type, a body that is one or more bullets with indented continuations), so a typo cannot silently drop an entry at release.
- **`check-pr`**, on pull requests only, judged against the merge base with the base branch:
  - **A fragment is required.** The pull request must add, edit or rename one.
  - **A user-visible extension change needs an extension fragment** under `changelog.d/vscode/`. A change under `editors/vscode/` counts unless it is a test, the changelog or readme, the lockfile, build configuration or a dotfile; a change to `package.json` counts only when it goes beyond `devDependencies`, `scripts` and `version`.
  - **The changelogs are not edited directly.** A pull request that changes `CHANGELOG.md` or `editors/vscode/CHANGELOG.md` fails, unless it is a release: one that adds a new `## [X.Y.Z]` section to that changelog. The title is never consulted.

  These are properties of the pull request, which a queue rebase cannot change, so the merge-queue run checks fragment validity only.

Waivers are commit trailers, because commit messages exist on the pull request and on its queue entry while a pull request body is not under review. A trailer belongs in the last paragraph of the message, beside `Closes #N`; the same words in the body waive nothing. It waives the rule for the whole pull request, whichever commit carries it. Say why after the keyword:

| Trailer                              | Waives                                                                                       |
|--------------------------------------|----------------------------------------------------------------------------------------------|
| `Changelog: none <reason>`           | Both fragment requirements, for a change with no user-visible effect (docs, CI, a refactor)  |
| `Changelog: amend-released <reason>` | The direct-edit rule (and the fragment requirement), to correct an already published section |

A pull request opened by a bot (Dependabot) is waived. `python3 scripts/changelog.py --help` lists the subcommands, `python3 scripts/changelog.py check-pr --base origin/main` runs the pull request check locally, and `python3 -m unittest discover -s scripts -p 'test_*.py'` runs the tests. The workflow has no `paths:` filter, so its `Changelog` context reports on every pull request and merge-queue entry; it is a required status check on `main`.

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
- [VS Code extension release setup](../editors/vscode/README.md#releasing)
