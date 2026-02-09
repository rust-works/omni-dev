# Known Limitations

## Context Window Limits for Large Branches

### Problem

When using `omni-dev git branch create pr` on branches with many commits, the AI request may fail with:

```
prompt is too long: XXXXX tokens > 200000 maximum
```

This occurs because omni-dev includes full diff content for all commits in the prompt sent to the AI model.

### Observed Behavior

When the token limit is exceeded:
- The AI call fails silently
- omni-dev falls back to a basic description
- The PR description contains the unfilled template with HTML comments still present
- A "📝 Commit Summary" section is appended with a list of commits

Example of fallback output:
```markdown
## Description
<!--
Provide a clear overview of what this PR does and why.
-->

## Changes Made
-
-
-

---
## 📝 Commit Summary
*This section was automatically generated based on commit analysis*

### Commits in this PR:
- `abc123` feat(scope): first commit
- `def456` fix(scope): second commit
...
```

### Workaround

For now, large branches require manual PR description writing or squashing commits before PR creation.

### Debugging

To identify if this is happening, run with debug logging:

```bash
RUST_LOG=omni_dev::cli::git=debug omni-dev git branch create pr --draft 2>&1 | grep -E "fallback|failed"
```

Look for:
```
AI PR generation failed, falling back to basic description
```

### Root Cause

The `generate_pr_content_with_context` function in `src/claude/client.rs` converts the full repository view (including all commit diffs) to YAML and sends it to the AI. For branches with:
- Many commits (e.g., 67 commits)
- Large diffs per commit

The combined token count can exceed Claude's 200K context window.

### Potential Solutions

1. **Truncate large diffs**: Limit each diff to N lines
2. **Summarize older commits**: Use `git diff --stat` for older commits, full diff only for recent ones
3. **Two-pass approach**: First summarize commits, then generate description
4. **Exclude binary/generated files**: Skip files that don't contribute meaningful context
5. **Token budget**: Estimate tokens per commit and stay within budget
6. **User control**: Add `--max-commits` or `--max-diff-lines` flags

### Related Files

- `src/claude/client.rs`: `generate_pr_content_with_context()`
- `src/cli/git.rs`: `generate_pr_content_with_client_internal()`
- `src/claude/prompts.rs`: `generate_pr_description_prompt_with_context()`
