---
name: commit-twiddle
description: Validates and corrects commit messages against project guidelines after a commit is created. Invoke automatically after every git commit to ensure messages conform to .omni-dev/commit-guidelines.md.
argument-hint: [commit-hash|range]
allowed-tools: Bash(mkdir *), Read, Write, Edit, "omni-dev *"
---

# Commit Twiddle

Validate and correct commit messages to conform to `.omni-dev/commit-guidelines.md`.

If `$ARGUMENTS` is provided, use it as the commit hash or range. Otherwise default to `HEAD`.

## Step 1 — Analyse

```bash
omni-dev git commit message view $ARGUMENTS
```

The output is self-describing. Read the `explanation.fields` section to understand all
available fields, then extract the values needed for Step 2.

Key fields to look at:
- `commits[].original_message` — the current message
- `commits[].analysis.detected_type` and `detected_scope` — what omni-dev detected
- `commits[].analysis.diff_file` — read this file to understand what actually changed
- `ai.scratch` — base path for temporary files

## Step 2 — Craft corrected messages

For each commit, write a corrected message that conforms to the guidelines. Constraints:

- **Type and scope** must match the actual changes — read the diff if uncertain
- **Scope** must be one of the values defined in `.omni-dev/scopes.yaml`
- **Subject line** must be lowercase, imperative mood, no trailing period, ≤72 chars total
- **No `Co-Authored-By` footers** — do not add AI attribution lines
- **Body** required for changes >50 lines or architectural changes; wrap at 72 cols

Write the amendments file to `<ai.scratch>/amendments-<random-8-hex>.yaml`:

```yaml
amendments:
  - commit: "<exactly 40 lowercase hex chars>"   # full SHA, not abbreviated
    message: |
      <type>(<scope>): <subject>

      <optional body wrapped at 72 cols>

      <optional footers — no Co-Authored-By>
```

Create the directory if the write fails:

```bash
mkdir -p <ai.scratch>
```

## Step 3 — Apply

```bash
omni-dev git commit message amend <ai.scratch>/amendments-<random-8-hex>.yaml
```

## Constraints

- The target commit must be at the **branch tip with no merge commit above it**. If a
  merge commit is present the amend will fail — work on the branch before merging.
- Use the **exact 40-character SHA** from the `commits[].hash` field. An abbreviated
  hash silently skips the amendment or errors.
- If the message is already correct, do nothing and say so.

## Troubleshooting

If `omni-dev` is not installed: https://crates.io/crates/omni-dev (requires ≥ v0.3.0)
