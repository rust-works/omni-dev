---
name: changelog
description: Writes changelog fragments (changelog.d/) following Keep a Changelog format, and assembles CHANGELOG.md from them at release time. Use when adding changelog entries, documenting changes, updating release notes, or preparing for a release. Triggers on terms like "changelog", "changelog fragment", "document changes", "release notes", "what changed".
---

# Changelog Management Skill

Pull requests never edit `CHANGELOG.md` or `editors/vscode/CHANGELOG.md`. Each
one adds a **fragment** file, and the release assembles the fragments into the
changelog with `scripts/changelog.py collect` (#2213, ADR-0097). The full
contributor guide is [`changelog.d/README.md`](../../../changelog.d/README.md).

## Adding an entry

Create one file per entry:

| Change to             | File                                      |
|-----------------------|-------------------------------------------|
| the crate             | `changelog.d/<id>.<type>[.<n>].md`        |
| the VS Code extension | `changelog.d/vscode/<id>.<type>[.<n>].md` |

- `<id>`: the issue number, or `+<slug>` for a pull request with no issue.
- `<type>`: the section the entry goes in.
- `<n>`: optional, for a second entry with the same id and type (`2213.fixed.2.md`).

| Type         | Section           | Use for                                      |
|--------------|-------------------|----------------------------------------------|
| `added`      | **Added**         | New features                                 |
| `changed`    | **Changed**       | Changes in existing functionality            |
| `deprecated` | **Deprecated**    | Soon-to-be removed features                  |
| `removed`    | **Removed**       | Now removed features                         |
| `fixed`      | **Fixed**         | Bug fixes                                    |
| `security`   | **Security**      | Vulnerability fixes                          |
| `ci`         | **CI/CD**         | Workflow, release and build-pipeline changes |
| `docs`       | **Documentation** | Documentation-only changes worth announcing  |

A change to both the crate and the extension adds one fragment to each
directory. A user-visible change under `editors/vscode/` must have a fragment in
`changelog.d/vscode/`.

## Entry format

The file body is the entry exactly as it reads in the changelog: one bullet with
a bold lead-in, the issue link, then the prose. Continuation lines and nested
bullets are indented two spaces. No heading (the type is the section) and no
blank lines.

```markdown
- **`worktrees push` no longer offers a force-push on a guessed verdict** ([#2175](https://github.com/rust-works/omni-dev/issues/2175)): in a shallow clone ...
```

Bad:

```markdown
### Fixed
- fixed push
```

To correct an entry that has not been released yet, edit that pull request's
fragment rather than adding another.

## Checking

```bash
python3 scripts/changelog.py check                          # every fragment is valid
python3 scripts/changelog.py check-pr --base origin/main    # this branch carries what CI requires
```

The `Changelog` CI check runs both. A change with no user-visible effect (docs,
CI, a refactor) carries a `Changelog: none <reason>` commit trailer instead of a
fragment. Correcting an already published section is the one direct edit to a
changelog, and needs a `Changelog: amend-released <reason>` trailer. Trailers go
in the last paragraph of a commit message.

## Releasing

```bash
python3 scripts/changelog.py collect --component crate  --version X.Y.Z --dry-run
python3 scripts/changelog.py collect --component crate  --version X.Y.Z
python3 scripts/changelog.py collect --component vscode --version A.B.C
```

`collect` writes `## [X.Y.Z] - <today>` with the sections in the order above and
entries by issue number, updates the crate changelog's compare links, carries any
legacy text under `[Unreleased]` into the section verbatim, and deletes the
fragments it consumed. Reconcile the fragments against the log first:

```bash
git log --oneline $(git describe --tags --abbrev=0 --match 'v[0-9]*')..origin/main
ls changelog.d/ changelog.d/vscode/
```

See [docs/RELEASE.md](../../../docs/RELEASE.md) for the whole release flow.
