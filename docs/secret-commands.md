# Fetching secrets with `<NAME>_COMMAND`

Every secret omni-dev reads from the environment or from the `env` map in
`~/.omni-dev/settings.json` accepts three spellings:

| Variable         | Meaning                                                    |
|------------------|------------------------------------------------------------|
| `NAME`           | The secret itself.                                         |
| `NAME_FILE`      | An absolute path to a file holding it ([ADR-0089](adrs/adr-0089.md)). |
| `NAME_COMMAND`   | A command whose standard output is the secret ([ADR-0090](adrs/adr-0090.md)). |

Set **one** of them. Two in the same place (both exported, or both in the same
settings.json map) is an error naming them. Across places the usual precedence
applies: the process environment beats settings.json, so an exported
`DATADOG_API_KEY_COMMAND` overrides a `DATADOG_API_KEY` in settings.json.

`_COMMAND` fetches the secret **on demand** from a store that does not keep it as
plaintext on disk. The secrets covered are the ones listed under each service's
own page (AI backends, Jev, Atlassian, Datadog, Gmail, Drive, Snowflake, the
browser bridge); `GMAIL_CLIENT_SECRET` is not one of them.

## What it protects, and what it does not

`_COMMAND` reliably removes the secret from disk, from your environment, and from
settings.json. Whether it also stops another process running as you from reading
the secret depends on the **store**, not on omni-dev: that process can read
settings.json, see the command, and run it.

- A store that **prompts on every use** makes that process cause a prompt you can
  see and refuse. That is the point of the feature.
- A store that does not prompt only keeps the secret off disk.

Stripping `*_COMMAND` from the environment of the `claude-cli` backend's nested
session is hygiene, not a boundary, for the same reason.

## Recipes

Prompting per use:

```sh
# 1Password CLI with the desktop app's biometric approval.
DATADOG_API_KEY_COMMAND='/opt/homebrew/bin/op read op://Private/datadog/api-key'
```

How long one approval lasts is 1Password's setting (per command or per terminal
session), not omni-dev's; check it before relying on it for the threat above.

```sh
# macOS keychain item with an empty trusted-application list (`-T ""`), so
# macOS asks before any program, `security` included, reads it.
security add-generic-password -s omni-dev-atlassian -a "$USER" -w -T ""
ATLASSIAN_API_TOKEN_COMMAND='/usr/bin/security find-generic-password -s omni-dev-atlassian -w'
```

The keychain prompt behaviour is macOS's and has not been verified here on every
macOS version; try `security find-generic-password` from a second terminal and
confirm it asks before you depend on it.

Keeps it off disk only (any process running as you can run these without a
prompt):

```sh
# The same `find-generic-password` command over an item created without
# `-T ""` reads without a prompt, because the item trusts `security` itself.
# Linux Secret Service, `pass`.
DATADOG_APP_KEY_COMMAND='/usr/bin/secret-tool lookup service omni-dev key datadog-app'
DATADOG_APP_KEY_COMMAND='/usr/bin/pass show omni-dev/datadog-app'
```

## How the command is run

- **No shell.** The value is split into arguments like a shell would split it
  (quotes and backslashes), then the program is run directly, so `;`, `|`,
  `$(…)` and `~` mean nothing. Write `sh -c '…'` yourself when you need a
  pipeline. The splitting follows POSIX rules, where a backslash escapes the next
  character, so on Windows put a path in single quotes (`'C:\Program Files\…\op.exe' read …`).
- **Absolute paths.** A bare program name is looked up on `PATH`. The daemon is
  started by launchd or systemd with a minimal `PATH`, so use an absolute path
  for anything the daemon reads (`SNOWFLAKE_TOKEN_COMMAND`, `OMNI_BRIDGE_TOKEN_COMMAND`).
  The error says so when the program is not found.
- **Input and output.** Standard input is `/dev/null`. Standard output, up to
  64 KiB, is the secret, with one trailing newline removed. Standard error is
  not shown on success; on failure the first 512 bytes are appended to the error.
- **Failure.** A non-zero exit, no output, or output that is not UTF-8 is an
  error. Errors name the variable and the program, never the arguments or the
  output. A failing helper's own standard error is shown, so a helper that echoes
  its arguments in an error message shows them.
- **Environment.** The command inherits your environment minus every omni-dev
  secret and its `_FILE`/`_COMMAND` companion.
- **Timeout.** 60 seconds, then the command is killed, along with anything it
  started when there is no terminal (the daemon, the MCP server, a pipe). With a
  terminal attached the helper stays in your foreground process group, so one that
  prompts on the terminal (`pass`, `gpg`'s curses pinentry) works; only the helper
  itself is killed on a timeout.
  A biometric prompt needs a person, so raise it if you are slow to approve:
  `OMNI_DEV_SECRET_COMMAND_TIMEOUT_SECS=120`.
- **Caching.** A successful result is reused for 300 seconds, by command, so one
  invocation (or a burst of MCP tool calls) prompts once; simultaneous callers
  share a single run. `OMNI_DEV_SECRET_COMMAND_TTL_SECS=0` turns the cache off
  (and prompts on every resolution); a rotated secret is picked up within the TTL.
  A failure is never cached.
- **`auth status`** reports a secret as configured when `NAME_COMMAND` is set,
  without running it.

## In settings.json

```json
{ "env": { "DATADOG_API_KEY_COMMAND": "/opt/homebrew/bin/op read op://Private/datadog/api-key" } }
```

The daemon does not inherit your shell, so this is where its secrets belong.
`settings.json` is read only from your home directory, so a repository you clone
cannot supply a command. It does mean a command in that file runs whenever a
command needs the secret; keep the file `0600`, as omni-dev writes it.

## Logging in

`auth login` writes the new credential to settings.json as plaintext, which
would replace your store. So when the map it would write to holds a
`NAME_COMMAND` for a secret it would write, it **refuses**, before opening a
browser or writing anything, and tells you which one. A `NAME_COMMAND` exported
in your environment is refused too, for `gmail auth login` and `drive auth login`,
because it would shadow the saved value. `drive auth login` also writes the client
secret, so a `DRIVE_CLIENT_SECRET_COMMAND` blocks it until you remove that line;
a login that skips command-fetched secrets is a possible follow-up. Store the new value in
your store (or remove the `_COMMAND` line) and run the command again. `auth
logout` removes the `NAME`, `NAME_FILE` and `NAME_COMMAND` entries.

## Not covered yet

Named Drive and Gmail accounts (`drive.accounts.<name>`, `gmail.accounts.<name>`)
accept `client_secret_file` and `refresh_token_file` but not a `_command` field.
Reading the OS keychain directly, without a command, is also not built; see
[ADR-0090](adrs/adr-0090.md).
