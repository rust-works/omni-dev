# Getting Started with omni-dev

omni-dev improves your git commit messages with AI. This guide takes you
from zero to your first AI-improved commit in under 10 minutes.

## What you'll do

1. Install the `omni-dev` binary.
2. Set up authentication for the default Anthropic backend.
3. Create a minimal `.omni-dev/` configuration directory.
4. Run `omni-dev git commit message twiddle` on a real commit range and
   apply the suggested improvement.

By the end you'll know the core `view` → `twiddle` → `check` workflow and
where to dig deeper.

## Prerequisites

- **Rust 1.80+** — install via [rustup.rs](https://rustup.rs/) if you
  don't have it. (`rustc --version` to check.)
- **Git** — any modern version.
- **A git repository** with at least one feature-branch commit ahead of
  `main`. (If you don't have one handy, create a scratch branch first;
  the "Your First Improvement" tutorial in
  [user-guide.md](user-guide.md#your-first-improvement) walks through
  that case.)

## 1. Install omni-dev

```bash
cargo install omni-dev
```

Verify the install:

```bash
omni-dev --version
```

If `omni-dev` isn't found, ensure `$HOME/.cargo/bin` is on your `PATH`.

Alternative install methods (Nix, binary cache) are documented in
[README.md#installation](../README.md#-quick-start).

## 2. Authenticate

omni-dev uses Anthropic's Claude API by default. Get a key from the
[Anthropic Console](https://console.anthropic.com/) and export it:

```bash
export CLAUDE_API_KEY="sk-ant-api03-..."
```

The Anthropic backend accepts any of these env vars (first match wins):
`CLAUDE_API_KEY`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`.

To make the export persistent, add it to your shell rc file:

```bash
echo 'export CLAUDE_API_KEY="sk-ant-api03-..."' >> ~/.zshrc   # zsh
echo 'export CLAUDE_API_KEY="sk-ant-api03-..."' >> ~/.bashrc  # bash
```

Using Bedrock, OpenAI, Ollama, or an already-authenticated Claude Code
CLI session instead? See
[AI Backend Selection](configuration.md#ai-backend-selection).

Full reference (`.env` files, CI/CD secrets, troubleshooting): see
[Authentication](configuration.md#authentication).

## 3. Initialise project context

omni-dev reads project conventions from a `.omni-dev/` directory at your
repo root. Create the minimum two files:

```bash
mkdir .omni-dev
```

**`.omni-dev/scopes.yaml`** — what parts of your codebase the AI can
reference in commit scopes:

```yaml
scopes:
  - name: "core"
    description: "Core application changes"
    examples:
      - "feat(core): add request middleware"
    file_patterns:
      - "src/**"
```

**`.omni-dev/commit-guidelines.md`** — your team's conventions in prose:

```markdown
# Commit Guidelines

Use conventional commits: `type(scope): description`.
Types we use: feat, fix, docs, chore, refactor, test.
```

That's enough for omni-dev to bias suggestions toward your project. For
the full schema (multiple scopes, `file_patterns`, local overrides,
monorepo setups) see the [Configuration Guide](configuration.md) and
[Configuration Best Practices](configuration-best-practices.md); the
[`.omni-dev/` Directory Contract](omni-dev-directory.md) is the canonical
reference for every recognised file, its format, and the precedence rules.

## 4. Improve your first commit

Make sure you're on a feature branch with at least one commit ahead of
`main`, and that `git status` is clean (omni-dev can't amend commits with
uncommitted changes).

Run `twiddle` against the range:

```bash
omni-dev git commit message twiddle 'origin/main..HEAD'
```

Quote the range — the `..` confuses some shells if left bare.

What to expect:

1. omni-dev prints model info and analyses each commit in the range.
2. For each commit it shows a suggested rewritten message with a
   before/after diff. For example:

   ```
   Before: wip auth fix
   After:  fix(auth): handle expired refresh tokens
   ```

3. You'll see a prompt: `Apply these amendments? [y/N]`. Press `y` to
   rewrite the commit messages in place (the commits are amended; their
   content is unchanged).

Verify with:

```bash
git log --oneline origin/main..HEAD
```

The subjects should now match the suggestions you approved.

## 5. Where to go next

- **Learn the full command set** — [user-guide.md](user-guide.md)
  starts with a "Your First Improvement" tutorial and works up through
  every command.
- **Configure for a real project** —
  [configuration.md](configuration.md) covers multi-scope `scopes.yaml`,
  monorepo layouts, and local overrides; pair it with
  [configuration-best-practices.md](configuration-best-practices.md).
- **Use a different AI backend** —
  [AI Backend Selection](configuration.md#ai-backend-selection) for
  Bedrock, OpenAI, Ollama, or claude-cli.
- **Generate a PR description after twiddling** — see the
  `create pr` section in [user-guide.md](user-guide.md).

## Troubleshooting quick links

- `CLAUDE_API_KEY not found` →
  [troubleshooting.md#api-key-problems](troubleshooting.md#api-key-problems)
- `Cannot amend commits with uncommitted changes` →
  [troubleshooting.md#git-repository-issues](troubleshooting.md#git-repository-issues)
- `No commits in range` →
  [troubleshooting.md#commit-analysis-problems](troubleshooting.md#commit-analysis-problems)
