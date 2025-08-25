allowed-tools: [Bash(mkdir *), Read, Write, Edit, "omni-dev *"]
argument-hint: [range]
description: Twiddle git commit messages
model: claude-sonnet-4

# Step 1
Run this command:

```bash
./target/debug/omni-dev git branch info
```

# Step 2
Analyse the result.  The result of the previous command is self describing.

# Step 3
If according to the results, a PR for this branch already exists, then stop.

If according to the result there are untracked changes then stop.

If according to the result the PR needs to be rebased on the remote main branch, then stop.

If a PR for this branch doesn't yet exist, then create a PR with a PR description based on the PR template in the result.
