---
name: changelog
description: Manages CHANGELOG.md entries following Keep a Changelog format. Use when adding changelog entries, documenting changes, updating release notes, or preparing for a release. Triggers on terms like "changelog", "document changes", "release notes", "what changed".
---

# Changelog Management Skill

This skill helps maintain CHANGELOG.md following [Keep a Changelog](https://keepachangelog.com/) format.

## Changelog Structure

```markdown
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [X.Y.Z] - YYYY-MM-DD

### Added
### Changed
### Deprecated
### Removed
### Fixed
### Security

[Unreleased]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z...HEAD
[X.Y.Z]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z-1...vX.Y.Z
```

## Categories

| Category          | Use For                                 |
|-------------------|-----------------------------------------|
| **Added**         | New features                            |
| **Changed**       | Changes in existing functionality       |
| **Deprecated**    | Soon-to-be removed features             |
| **Removed**       | Now removed features                    |
| **Fixed**         | Bug fixes                               |
| **Security**      | Vulnerability fixes                     |
| **Documentation** | Documentation-only changes              |
| **Refactored**    | Code changes that don't affect behavior |

## Entry Format

Each entry should:
- Start with `**Bold Feature Name**:` followed by description
- Use sub-bullets for implementation details
- Be written in past tense for releases, present for unreleased

### Good Example

```markdown
### Added
- **Commit Message Validation Command**: New `check` command for validating commit messages
  - AI-powered analysis with configurable severity levels
  - Multiple output formats (text, JSON, YAML) for CI/CD integration
  - Smart exit codes for pipeline integration
```

### Bad Example

```markdown
### Added
- Added check command
- It validates commits
```

## Key Learnings

### Incremental Updates
Add entries to `[Unreleased]` as features are merged, not all at once during release. This:
- Makes release prep faster
- Ensures nothing is missed
- Provides better commit-to-changelog traceability

### Version Links
Always maintain comparison links at the bottom of the file:

```markdown
[Unreleased]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z...HEAD
[X.Y.Z]: https://github.com/rust-works/omni-dev/compare/vX.Y.Z-1...vX.Y.Z
```

When releasing:
1. Update `[Unreleased]` link to compare against new version
2. Add new version's comparison link

### Gathering Changes

To see what changed since last release:

```bash
# Get last release tag
git describe --tags --abbrev=0

# Show commits since last release
git log --oneline $(git describe --tags --abbrev=0)..HEAD

# Show detailed commit info
git show <commit-hash> --stat
```

## Instructions

1. Read current CHANGELOG.md
2. Identify the appropriate category for changes
3. Write clear, descriptive entries with details
4. For releases, move `[Unreleased]` content to new version section
5. Update version comparison links
