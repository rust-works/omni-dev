# Shell Completion

`omni-dev completions <shell>` prints a shell completion script to stdout. The
script is generated from the live clap command tree, so it stays in sync with
the CLI surface across releases without a separate maintenance burden.

Supported shells: `bash`, `zsh`, `fish`, `powershell`, `elvish`.

The subcommand is hidden from the top-level `--help` listing to keep the
primary help block focused on day-to-day commands, but it remains discoverable
via `omni-dev help-all`.

## Quick check

```bash
omni-dev completions bash | head
```

If you see a `_omni-dev()` function definition, the binary is working. The
remaining sections cover how to install the script so your shell actually
loads it.

## Bash

### Per-user (recommended)

```bash
# In ~/.bashrc:
eval "$(omni-dev completions bash)"
```

This regenerates the completion script every time a new bash session starts,
so upgrading `omni-dev` is enough — there is no separate file to refresh.

### System-wide

```bash
omni-dev completions bash | sudo tee /etc/bash_completion.d/omni-dev > /dev/null
```

Requires the `bash-completion` package installed on the system. New shells
pick it up automatically.

## Zsh

Zsh discovers completion functions via `$fpath` and loads them through
`compinit`. The file must be named `_omni-dev` (underscore prefix matching the
command name) and live in a directory on `$fpath`.

### Per-user (recommended)

The naive `omni-dev completions zsh > "${fpath[1]}/_omni-dev"` recipe assumes
`$fpath[1]` is user-writable. On most systems it is not — it tends to be
`/usr/share/zsh/site-functions`, owned by root. Use a directory you own
instead:

```bash
mkdir -p ~/.zsh/completions
omni-dev completions zsh > ~/.zsh/completions/_omni-dev
```

Then add this to `~/.zshrc` **before** `compinit` runs:

```zsh
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

Open a new shell (or `exec zsh`) and tab-completion will work.

### Oh My Zsh

Oh My Zsh adds `~/.oh-my-zsh/completions` to `$fpath` automatically, so you
can skip the `$fpath` edit:

```bash
mkdir -p ~/.oh-my-zsh/completions
omni-dev completions zsh > ~/.oh-my-zsh/completions/_omni-dev
```

### Verifying without restarting

```zsh
unfunction _omni-dev 2>/dev/null
autoload -Uz _omni-dev
omni-dev <Tab>
```

If completion fires, the installation worked.

### Troubleshooting

- **"_omni-dev: function definition file not found"** — the file is not on
  `$fpath`. Run `echo $fpath` and confirm the directory you wrote to is
  listed; if not, the `fpath=(...)` line in `.zshrc` is missing or runs
  after `compinit`.
- **No completions at all** — clear the compinit cache and retry:
  `rm -f ~/.zcompdump*; exec zsh`.

## Fish

Fish loads completions from `~/.config/fish/completions/<command>.fish`
automatically:

```fish
omni-dev completions fish > ~/.config/fish/completions/omni-dev.fish
```

No `fish` config change is required. The completion is available immediately
in new shells (and in existing shells after `source` of the file).

## PowerShell

PowerShell uses `Register-ArgumentCompleter`. For the current session:

```powershell
omni-dev completions powershell | Out-String | Invoke-Expression
```

For persistence across sessions, append the same line to your `$PROFILE`:

```powershell
Add-Content $PROFILE 'omni-dev completions powershell | Out-String | Invoke-Expression'
```

Then reopen the shell or `. $PROFILE`.

## Elvish

Elvish loads modules from `~/.config/elvish/lib/`:

```bash
omni-dev completions elvish > ~/.config/elvish/lib/omni-dev.elv
```

Then `use omni-dev` from `~/.config/elvish/rc.elv` to load it on shell start.

## Upgrading

When you upgrade `omni-dev`, regenerate any installed completion files so they
reflect new subcommands and flags. The bash `eval "$(...)"` recipe does this
automatically; the file-based recipes do not.

A simple refresh script:

```bash
omni-dev completions zsh > ~/.zsh/completions/_omni-dev
omni-dev completions fish > ~/.config/fish/completions/omni-dev.fish
```
