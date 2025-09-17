# omni-dev Commit Guidelines

This project follows conventional commit format with specific requirements:

## Commit Format
```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

## Types
- `feat`: New features or enhancements
- `fix`: Bug fixes
- `docs`: Documentation changes
- `refactor`: Code refactoring without behavior changes
- `chore`: Maintenance tasks, dependency updates
- `test`: Test additions or modifications
- `ci`: CI/CD pipeline changes
- `build`: Build system changes

## Scopes
Use scopes defined in `.omni-dev/scopes.yaml` to indicate the area of change.

## Body Guidelines
For significant changes, include:
- What was changed and why
- Impact on users or developers
- Any breaking changes or migration notes
- References to issues or tickets

## Examples
```
feat(claude): add contextual intelligence for commit message improvement

Implements Phase 3 of the twiddle command enhancement with multi-layer
context discovery including project conventions, branch analysis, and
work pattern detection. This enables more comprehensive and detailed
commit messages similar to the Claude Code template workflow.

- Add project context discovery from .omni-dev/ configuration
- Implement branch naming pattern analysis
- Add work pattern detection across commit ranges
- Enhance Claude prompting with contextual intelligence
- Support verbosity levels based on change significance

Closes #12
```
