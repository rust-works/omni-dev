# omni-dev Commit Guidelines

This project follows conventional commit format with specific requirements.

## Severity Levels

| Severity | Sections                                                                |
|----------|-------------------------------------------------------------------------|
| error    | Commit Format, Types, Scopes, Subject Line, Accuracy, Breaking Changes |
| warning  | Body Guidelines                                                         |
| info     | Subject Line Style                                                      |

## Commit Format

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

Multiple scopes are allowed when a commit spans more than one area.
Separate scopes with a comma and no space:

```
<type>(<scope1>,<scope2>): <description>
```

## Types

Required. Must be one of:

| Type       | Use for                                               |
|------------|-------------------------------------------------------|
| `feat`     | New features or enhancements to existing features     |
| `fix`      | Bug fixes                                             |
| `docs`     | Documentation changes only                            |
| `refactor` | Code refactoring without behavior changes             |
| `chore`    | Maintenance tasks, dependency updates, config changes |
| `test`     | Test additions or modifications                       |
| `ci`       | CI/CD pipeline changes                                |
| `build`    | Build system or external dependency changes           |
| `perf`     | Performance improvements                              |
| `style`    | Code style changes (formatting, whitespace)           |

## Scopes

Required. Use scopes defined in `.omni-dev/scopes.yaml`:

- `ci` - CI/CD pipelines and GitHub Actions workflows
- `claude` - Claude AI client implementation and integration
- `cli` - Command-line interface and argument parsing
- `git` - Git operations and repository analysis
- `data` - Data structures and serialization
- `docs` - Documentation and planning
- `release` - Release process, versioning, and publishing
- `scopes` - Commit scope definitions and configuration
- `workflows` - GitHub Actions workflow files

For multi-scope commits, the scopes are correct when each listed scope
matches at least one modified file. Do not flag scopes as incorrect
when the commit legitimately spans multiple areas.

## Subject Line

- Keep under 72 characters total
- Use imperative mood: "add feature" not "added feature" or "adds feature"
- Be specific: avoid vague terms like "update", "fix stuff", "changes"

## Subject Line Style

- Use lowercase for the description
- No period at the end

## Accuracy

The commit message must accurately reflect the actual code changes:

- **Type must match changes**: Don't use `feat` for a bug fix, or `fix` for new functionality
- **Scope must match files**: The scope should reflect which area of code was modified
- **Description must be truthful**: Don't claim changes that weren't made
- **Mention significant changes**: If you add error handling, logging, or change behavior, mention it

Only flag accuracy errors when the commit message is clearly and
materially wrong. Do not flag minor terminology differences,
language-specific semantic debates, or cases where the description
is substantially correct even if slightly imprecise. Before reporting
an issue, verify your reasoning is internally consistent — if your
own explanation concludes the commit is actually correct, do not
report it.

## Body Guidelines

For significant changes (>50 lines or architectural changes), include a body:

- Explain what was changed and why
- Describe the approach taken
- Note any breaking changes or migration requirements
- Use bullet points for multiple related changes
- Reference issues in footer: `Closes #123` or `Fixes #456`

## Breaking Changes

For breaking changes:
- Add `!` after type/scope: `feat(cli)!: change output format`
- Include `BREAKING CHANGE:` footer with migration instructions

## Examples

### Simple change
```
fix(git): handle detached HEAD in branch analysis
```

### Feature with body
```
feat(claude): implement contextual prompting for commit analysis

Adds context-aware system prompts that incorporate project scopes,
branch analysis, and file-level architectural understanding to
produce higher-quality commit message suggestions.

- Add CommitContext with project, branch, and file context
- Implement scope-aware prompt generation
- Extract file purpose and architectural layer classification

Closes #85
```

### Documentation
```
docs(docs): add ADR for context intelligence design
```

```
docs(docs): add architecture overview document
```

### Multiple scopes
```
feat(cli,claude): add twiddle contextual options
```

```
feat(git,data): integrate branch analysis with commit context

Wires branch detection into the commit analysis pipeline and
exposes branch context through the data structures.

- Add BranchContext with work type detection
- Integrate branch parsing into GitRepository
- Surface branch context in YAML output
```

### Breaking change
```
feat(cli)!: change commit check output format

BREAKING CHANGE: The check command now returns structured YAML
instead of plain text. Update scripts that parse the output
to use the new format.
```
