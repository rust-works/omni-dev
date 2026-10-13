# Privacy Policy

omni-dev is an open-source, locally-run command-line tool. This policy
covers the Google APIs (Drive and Gmail) it can access on your behalf via
OAuth.

The Drive and Gmail integrations have moved out of omni-dev to the separate
[gwi](https://github.com/rust-works/gwi) project
([#2203](https://github.com/rust-works/omni-dev/issues/2203)). Current omni-dev
releases no longer contact Google's APIs; `omni-dev drive` and `omni-dev gmail`
remain only as stubs that print a pointer to gwi. This policy continues to
describe how earlier omni-dev releases that included those integrations handled
your data, and the tokens and archives they left on your machine.

## What this is

omni-dev runs entirely on your own machine as a CLI binary. There is no
omni-dev server: when you run `omni-dev drive ...` or `omni-dev gmail ...`,
the tool talks directly from your machine to Google's APIs using your own
Google account credentials. No Drive or Gmail data passes through, or is
visible to, the project maintainers or any third party.

## What data is accessed

Depending on the scopes you grant during `omni-dev drive auth login` /
`omni-dev gmail auth login`, the tool can read and, for scopes you opt into,
write Drive file content/metadata and Gmail messages — see
[docs/drive.md](https://github.com/rust-works/gwi/blob/main/docs/drive.md) and [docs/gmail.md](https://github.com/rust-works/gwi/blob/main/docs/gmail.md) for exactly what
each command and scope does. Data is used only to carry out the command you
invoked (e.g. searching Drive, reading a message, syncing mail to local
`.eml` files) and is not collected, aggregated, or transmitted anywhere by
omni-dev itself.

## What is stored, and where

- **OAuth refresh tokens** for Drive/Gmail are saved locally at
  `~/.omni-dev/settings.json`, written with file permissions restricted to
  your own user account (`0600`). Access tokens are never written to disk —
  only the refresh token persists, and it's used to mint short-lived access
  tokens on demand.
- **Gmail message content**, if you use `gmail sync`, is written as `.eml`
  files to a local directory you choose. No other command persists message
  or file content beyond the current run.
- **A local request log** (`docs/log.md`) records that a request happened
  (endpoint, timing, status) for debugging your own runs. It never logs
  auth tokens/headers, and request/response bodies are only recorded if you
  explicitly opt in (`OMNI_DEV_LOG_BODIES=1`), which is off by default.

Nothing above is sent to the project maintainers, an analytics service, or
any server other than Google's own APIs.

## Third-party AI access

Earlier omni-dev releases optionally exposed Drive/Gmail commands to an AI
assistant (e.g. Claude) via a local MCP server (`omni-dev-mcp`, see
[docs/mcp.md](mcp.md)); those tools now live in gwi's `gwi-mcp`.
This only runs if you explicitly configure your AI client to launch it, and
it communicates over local stdio — it is your own local integration, not a
channel omni-dev opens on your behalf.

## Revoking access

You can revoke omni-dev's access at any time from your Google Account's
[Third-party apps & services](https://myaccount.google.com/permissions)
page, and delete the locally stored refresh token by removing the
corresponding entry from `~/.omni-dev/settings.json`.

## Contact

Questions about this policy or the project's handling of data can be raised
via [GitHub Issues](https://github.com/rust-works/omni-dev/issues) on this
repository.
