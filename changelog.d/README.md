# Changelog fragments

Pull requests do not edit `CHANGELOG.md` or `editors/vscode/CHANGELOG.md`.
Each one adds its own file here instead, and the release assembles the files
into the changelog with `scripts/changelog.py collect`
([#2213](https://github.com/rust-works/omni-dev/issues/2213),
[ADR-0097](../docs/adrs/adr-0097.md)). Two pull requests that each add a
fragment never touch the same file, so they never conflict, and a fragment
cannot land inside a section a release has already closed
([#2129](https://github.com/rust-works/omni-dev/issues/2129)).

## Adding a fragment

| Change to             | Fragment                                  | Assembled into                |
|-----------------------|-------------------------------------------|-------------------------------|
| the crate             | `changelog.d/<id>.<type>[.<n>].md`        | `CHANGELOG.md`                |
| the VS Code extension | `changelog.d/vscode/<id>.<type>[.<n>].md` | `editors/vscode/CHANGELOG.md` |

| Part     | Meaning                                                                                       |
|----------|-----------------------------------------------------------------------------------------------|
| `<id>`   | the issue number (`2213`), or `+<slug>` (`+fix-typo`) for a pull request with no issue        |
| `<type>` | the section: `added`, `changed`, `deprecated`, `removed`, `fixed`, `security`, `ci` or `docs` |
| `<n>`    | optional positive integer for a second entry of the same id and type (`2213.fixed.2.md`)      |

`ci` renders as `### CI/CD` and `docs` as `### Documentation`; the others as
their Keep a Changelog headings. A change to both the crate and the extension
adds one fragment to each directory.

The file body is the entry exactly as it reads in the changelog: one bullet
(`- `) with the bold lead-in and the prose, and any continuation or nested
bullet indented two spaces. No section heading (the type is the section) and
no blank lines.

```markdown
- **`worktrees push` no longer offers a force-push on a guessed verdict** ([#2175](https://github.com/rust-works/omni-dev/issues/2175)): ...
```

A follow-up that corrects an entry not yet released edits **that pull
request's fragment**, so it conflicts with nothing.

## What CI checks

The `Changelog` check (`.github/workflows/changelog.yml`) runs:

- `python3 scripts/changelog.py check`: every fragment's name, type and body.
  Anything in these directories other than `README.md` (and `vscode/`) that is
  not a valid fragment fails, so a typo (`2213.chnaged.md`) cannot silently drop
  an entry at release.
- `python3 scripts/changelog.py check-pr` on a pull request, against the merge
  base:
  - it must add, edit or rename a fragment;
  - a user-visible change under `editors/vscode/` (anything but tests, the
    changelog and readme, the lockfile, build configuration, dotfiles and the
    dev-only keys of `package.json`) must add a fragment under
    `changelog.d/vscode/`;
  - it must not edit `CHANGELOG.md` or `editors/vscode/CHANGELOG.md` directly;
  - a release must consume every fragment of its component.

  On a merge-queue entry only the last two run (`check-pr --queue`): a rebase
  cannot change which files a pull request adds, but it can bring in a fragment
  that merged while a release was waiting in the queue.

Run it locally before pushing:

```bash
python3 scripts/changelog.py check-pr --base origin/main
```

### Waivers

Waivers are commit trailers, in the last paragraph of any commit on the branch:

| Trailer                              | Waives                                                                                     |
|--------------------------------------|--------------------------------------------------------------------------------------------|
| `Changelog: none <reason>`           | the fragment requirements, for a change with no user-visible effect (docs, CI, a refactor) |
| `Changelog: amend-released <reason>` | the direct-edit rule and the crate-fragment requirement, to correct a published section    |

`amend-released` covers published sections only: an edit under `[Unreleased]`
still fails, and an extension change still needs its fragment. Pull requests
opened by a bot (Dependabot) are waived. A release pull request is recognised by
the new `## [X.Y.Z]` section it adds to the changelog it releases, never by its
title; it waives the fragment requirement, and the extension fragment only when
it is the extension's release.

## Releasing

```bash
python3 scripts/changelog.py collect --component crate  --version X.Y.Z --dry-run
python3 scripts/changelog.py collect --component crate  --version X.Y.Z
python3 scripts/changelog.py collect --component vscode --version A.B.C
```

`collect` renders `## [X.Y.Z] - DATE` with its sections in the order above,
entries by ascending issue number (`+slug` entries last), updates the
`[Unreleased]` / `[X.Y.Z]` compare links (the crate's changelog only; the
extension's has none) and deletes the consumed fragments. See
[docs/RELEASE.md](../docs/RELEASE.md).

## The legacy `[Unreleased]` text

Both changelogs' `[Unreleased]` sections predate fragments. At each one's first
release, `collect` carries whatever is under `## [Unreleased]` into the new
section verbatim, **after** the fragment sections, and leaves an empty
`[Unreleased]` behind, so that first section may repeat a `###` heading. From
then on the changelogs are assembled purely from fragments.
