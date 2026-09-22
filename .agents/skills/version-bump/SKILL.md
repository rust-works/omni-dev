---
name: version-bump
description: Determines appropriate semantic version bumps based on changes. Use when deciding version numbers, evaluating breaking changes, or planning releases. Triggers on terms like "version", "semver", "breaking change", "major/minor/patch".
---

# Semantic Versioning Skill

This skill helps determine appropriate version bumps following [Semantic Versioning](https://semver.org/).

## Version Format

```
MAJOR.MINOR.PATCH
```

- **MAJOR**: Breaking changes
- **MINOR**: New features, backwards compatible
- **PATCH**: Bug fixes, backwards compatible

## Version Bump Decision Tree

### MAJOR (X.0.0) - Breaking Changes After 1.0

For versions at or above 1.0.0, bump MAJOR when you make incompatible API changes:

- Removed public functions, methods, or types
- Changed function signatures (parameters, return types)
- Renamed public APIs
- Changed default behavior that breaks existing usage
- Removed CLI flags or changed their meaning
- Changed configuration file format incompatibly

### MINOR (X.Y.0) - New Features and Pre-1.0 Breaking Changes

Bump MINOR when you add functionality in a backwards compatible manner, or
when you make a breaking change before 1.0.0:

- New commands or subcommands
- New CLI flags
- New configuration options
- New output formats
- New integrations or providers

### PATCH (0.0.X) - Bug Fixes

Bump PATCH when you make backwards compatible bug fixes:

- Fix incorrect behavior
- Fix crashes or errors
- Performance improvements (no API changes)
- Documentation fixes
- Internal refactoring (no behavior changes)

## Quick Reference

| Change Type                      | Version Bump               |
|----------------------------------|----------------------------|
| Breaking API change              | MAJOR (≥1.0), MINOR (<1.0) |
| Removed feature                  | MAJOR (≥1.0), MINOR (<1.0) |
| New command/feature              | MINOR                      |
| New CLI flag                     | MINOR                      |
| New provider/integration         | MINOR                      |
| Bug fix                          | PATCH                      |
| Performance fix                  | PATCH                      |
| Documentation only               | PATCH                      |
| Refactoring (no behavior change) | PATCH                      |

## Pre-1.0 Versioning

For versions < 1.0.0 (like this project):
- Use MINOR for breaking changes and new features
- Use PATCH for bug fixes, documentation, and refactoring without behavior changes

## Instructions

1. Review all changes since last release:
   ```bash
   git log --oneline $(git describe --tags --abbrev=0)..HEAD
   ```

2. Check for breaking changes:
   - Removed or renamed public APIs?
   - Changed default behaviors?
   - Incompatible configuration changes?

3. If breaking changes exist -> MINOR bump before 1.0.0; MAJOR bump from 1.0.0 onward

4. If new features exist -> MINOR bump

5. If only fixes/refactoring -> PATCH bump

## Version Update Locations

When bumping version, update:

1. **Cargo.toml** - `version = "X.Y.Z"`
2. **Cargo.lock** - Refresh the `omni-dev` package version (for example, by running `cargo check`) and include the lockfile change
3. **CHANGELOG.md** - Add `## [X.Y.Z] - YYYY-MM-DD` section
4. **Version links** - Update comparison URLs at bottom of CHANGELOG.md
