allowed-tools: [Bash(mkdir *), Read, Write, Edit, ".claude/commands/twiddle-msg *"]
argument-hint: [range]
description: Twiddle git commit messages
model: claude-sonnet-4

# Step 1
Run this command:

```bash
.claude/commands/twiddle-msg view $ARGUMENTS
```

# Step 2
Analyse the result.  The result of the previous command is self describing.

# Step 3
Craft new commit messages for each commit and overwrite them to `.ai/scratch/amendments-<random-hash>.yaml`.

Where `<random-hash>` is a random hexadecimal hash of length 8.

Assume the `.ai/scratch` direcotry exists and try creating the directory if writing the file fails.

The file must conform to the following schema (validation required)
```yaml
amendments:                    # required, non-empty array
  - commit: "<40-hex-sha>"     # required, exactly 40 lowercase hex
    message: |                 # required; Conventional Commit
      <subject line>
      
      <wrapped body at 72 cols>
      
      <optional footers>
```

# Step 4
Run this command:

```bash
.claude/commands/twiddle-msg amend .ai/scratch/amendments-<random-hash>.yaml
```
