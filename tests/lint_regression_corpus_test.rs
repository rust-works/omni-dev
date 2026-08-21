#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Regression corpus for the deterministic `lint_message` rules (#1474).
//!
//! This is a **hermetic fixture**, not a live walk of this repo's git
//! history: the corpus below is the literal first-line subjects (plus one
//! full multi-line message) of 399 real, non-merge commits from a pinned
//! range of this repo's own history (`a28f9598..466cf0fc`, the `main` tip
//! at the time this test was written). It is embedded as data rather than
//! read via `git2` at test time so the test neither depends on this
//! checkout having that history available (CI's default `actions/checkout`
//! is a shallow, depth-1 clone) nor drifts as new commits land on `main`
//! after this test merges.
//!
//! The expected pass/fail split was captured empirically by running the
//! implementation against the real range and inspecting every flagged
//! commit — it is not copied from the issue's approximate 59-over-72/
//! 28-over-80 counts, which used a simpler methodology (subject length
//! only). Breakdown of the 33 known exceptions:
//!
//! - **29 `subject-length`** (>80 chars) — the dominant category, exactly
//!   what the issue's measurement predicted.
//! - **1 `blank-line-after-subject`** — the issue's own headline example (a
//!   clean 61-char subject, non-blank line 2 folds the whole first
//!   paragraph into `%s`).
//! - **2 `unknown-scope`** — `settings`, `deps` (a dependabot commit):
//!   scopes used at commit time that are no longer (or never were) in the
//!   *current* `.omni-dev/scopes.yaml`. This is `scopes.yaml` drift, not a
//!   lint bug — explicitly out of scope for #1474 (`scopes.yaml` repair is
//!   #1468). A third entry, `92bfe968` (scope `coverage`), moved to
//!   [`PASSING_SUBJECTS`] once #1468 added `coverage` as a real scope.
//! - **1 `format`** — an external contributor's one-off PR ("Add MCP
//!   Toplist rank badge") that never followed conventional commit format.
//!
//! This is what proves the rules encode existing practice rather than a
//! new standard: every entry below has a one-line justification, and the
//! corpus is the *total* set — [`PASSING_SUBJECTS`] must lint clean and
//! [`FAILING_FIXTURES`] must fail with exactly its stated rule, with
//! nothing outside either list.

use omni_dev::data::context::CommitRules;
use omni_dev::git::lint_message;

/// Real, non-merge commit subjects from the pinned corpus range that are
/// expected to pass every rule cleanly. One entry per commit,
/// `(short_hash, subject)` — the hash is for failure-message readability
/// only, not used for matching.
#[rustfmt::skip]
const PASSING_SUBJECTS: &[(&str, &str)] = &[
    ("0cf5dfb1", "docs(docs,vscode): revert the session-priority reorder docs"),
    ("8ec45c02", "fix(vscode): rank working above idle in the Claude session cue"),
    ("436b4173", "docs(docs,release,vscode): document the behind-origin/main indicator"),
    ("b69fd297", "feat(vscode): add a passive behind-origin/main indicator to the tree"),
    ("9abc0573", "feat(cli): show behind-origin/main divergence in worktrees tree"),
    ("1ce6785f", "feat(daemon): compute divergence from the remote default branch"),
    ("a346bf5a", "docs(vscode): add changelog entry for dropped rebase confirmation modal"),
    ("47c69ccb", "docs(docs): sync worktrees-service.md with ADR-0062's dropped rebase modal"),
    ("a1740933", "docs(docs): add ADR-0062, dropping the rebase confirmation modal"),
    ("01d8f5e3", "feat(vscode): drop the rebase confirmation modal"),
    ("5eab7d04", "docs(docs,vscode): document the session-priority reorder"),
    ("90a122c8", "fix(vscode): rank idle above working in the Claude session cue"),
    ("fe815357", "fix(sessions,vscode): correct set_model claims, dedupe visible-worktree filter"),
    ("7ea48d1c", "fix(sessions): reflect a model switch instantly via set_model"),
    ("b0f46771", "fix(vscode): exclude hidden worktrees from the repo model marker"),
    ("332482fd", "fix(sessions): track model changes across turns in claude-wrap"),
    ("75b06a5f", "docs(docs,vscode): document the Claude model-family marker"),
    ("67a4db7b", "feat(vscode): show Claude model marker on worktree and repo rows"),
    ("c85b9b38", "feat(vscode): add pure builders for the Claude model-family marker"),
    ("e62db10d", "fix(git): preserve the spawn-failure reason in a push rejection"),
    ("3ad067a5", "test(git,cli,daemon): cover the push engine's failure and fallback paths"),
    ("fee8463f", "docs(docs,release): add ADR-0061 and document the worktrees push op"),
    ("27bb1f10", "feat(vscode): add the Push (force-with-lease) row action"),
    ("7b98847a", "feat(cli): add omni-dev worktrees push"),
    ("ec3c7b52", "feat(daemon): add a two-phase worktrees push op"),
    ("68c5eb10", "feat(git): add a worktree push engine that force-pushes with a lease"),
    ("d1684cc5", "refactor(git,cli): extract shared batch-worktree primitives"),
    ("8af5596a", "docs(docs,vscode): document the Open GitHub Repository command"),
    ("2f0f1346", "feat(vscode): add Open GitHub Repository to the worktrees tree"),
    ("4b3c81fa", "feat(vscode): add pure builders for a repository's github.com page"),
    ("8630489a", "chore(release, cargo): bump base64 from 0.22.1 to 0.23.0"),
    ("a6b1b12f", "chore(cargo): bump dependencies in rust-minor-patch group"),
    ("6a8f21c5", "feat(vscode): offer and execute Rebase on main on the main worktree row"),
    ("a9a43bd6", "docs(release): record main-working-tree rebase change in changelog"),
    ("04749d24", "docs(docs): sync worktrees-service.md and CLAUDE.md with ADR-0060"),
    ("e5db4d43", "docs(docs): add ADR-0060 superseding ADR-0055 §3 on rebasing main"),
    ("7a26cc76", "docs(daemon): note the main working tree is no longer skipped by rebase"),
    ("42423fb1", "feat(cli): surface main-working-tree rebasing in worktrees rebase"),
    ("8e437686", "feat(git): allow rebasing the repository's main working tree"),
    ("5e884ddb", "docs(release): record ADF schema 56.1.18 snapshot bump in changelog"),
    ("c44fa74a", "docs(atlassian): update ADF schema snapshot to 56.1.18"),
    ("7be2e6dc", "chore(release): prepare vscode extension release v0.8.0"),
    ("5ad874c5", "docs(docs,vscode): document the row icon's glyph and colour rules"),
    ("bbb407cc", "fix(vscode): keep the current-window tick visible during a rebase"),
    ("9332e416", "fix(vscode): colour the current-window tick like the open dot"),
    ("fec4f785", "docs(docs,vscode): document the Copy PR URL command"),
    ("bbd104fc", "feat(vscode): add a Copy PR URL command to the worktrees tree"),
    ("90a6974a", "refactor(vscode): route the missing-extension copy through one helper"),
    ("55495957", "feat(vscode): add a pure clipboard model for pull request URLs"),
    ("bfcd6212", "refactor(vscode): expose the branch label and PR scope key for reuse"),
    ("aa7ed099", "docs(docs,release,vscode): document per-row tree icon colours"),
    ("022c4fe3", "feat(vscode): add commands to set and clear row colours"),
    ("255d90bc", "feat(vscode): resolve a per-row icon colour tag"),
    ("c58b0c96", "refactor(vscode): extract row icon resolution into a pure module"),
    ("ad81d5cc", "test(daemon): observe the PR-poll debounce deadline by condition, not by clock"),
    ("6abb4efc", "chore(release): prepare release v0.39.0"),
    ("1f0e95ae", "chore(release): prepare vscode extension release v0.7.0"),
    ("69824f43", "docs(docs): correct stale worktrees op and CLI lists in CLAUDE.md"),
    ("958b7d85", "docs(git): drop redundant explicit link targets in RebaseResult docs"),
    ("767907fb", "docs(docs,release): add ADR-0059 and document the daemon-hosted rebase"),
    ("1e2cacc5", "feat(vscode): drive Rebase on main through the daemon, not a terminal"),
    ("09229330", "feat(daemon): host worktree rebase behind a two-phase op"),
    ("9bafbaad", "feat(git,cli): leave rebase conflicts in place, resolve the git binary"),
    ("34bc3643", "docs(docs,release): document the growth-precedence fix"),
    ("ce4c6d32", "fix(sessions): stop transcript growth clobbering a reported state"),
    ("48505d76", "docs(docs,release): document the register --repo-name rename"),
    ("863690e3", "test(cli): pin that no subcommand arg shadows a global arg id"),
    ("46fcbf25", "fix(cli)!: rename worktrees register --repo to --repo-name"),
    ("01818ebc", "test(scopes): assert commit-guidelines scope list matches scopes.yaml"),
    ("e33ce118", "docs(scopes): sync commit-guidelines scope list with scopes.yaml"),
    ("86f48f06", "test(cli,daemon): cover the reload CLI client and its audit line"),
    ("34a99d79", "docs(docs,release,vscode): document the worktrees reload op"),
    ("84b3c74c", "feat(cli): add worktrees reload"),
    ("73131dac", "feat(vscode): add Reload Window to the worktrees tree"),
    ("4a494c5a", "feat(daemon): add a reload op and directive to the worktrees service"),
    ("fedf20ed", "docs(docs,release): document the sessions subscribe op"),
    ("17a635df", "feat(vscode): push Claude session cues instead of polling per window"),
    ("b66311e7", "feat(vscode): generalize the daemon subscription client for sessions"),
    ("9114f159", "feat(sessions): stream live session state via a subscribe op"),
    ("6df43624", "feat(sessions): add a change-notify to the session registry"),
    ("c7c9d712", "test(cli,daemon): cover the reposition CLI and the non-macOS backend"),
    ("5636e001", "fix(daemon): keep the reposition backend building on non-macOS"),
    ("913ea36d", "docs(docs,release): add ADR-0058 and document the reposition op"),
    ("22b23dec", "feat(vscode): add Reposition Windows to Match to the tree view"),
    ("9231f2f8", "feat(cli): add worktrees reposition to match window geometry"),
    ("3d684483", "feat(daemon): reposition open VS Code windows via Accessibility"),
    ("4cfd24b5", "docs(docs): document the tree-view rebase entry point"),
    ("37d31534", "feat(vscode): add Rebase on main to the worktrees tree view"),
    ("61538469", "fix(daemon): stabilize worktree removal test by fixing directory creation race"),
    ("32a10df4", "test(cli): cover the claude-wrap dispatch arm and the shim-write failure"),
    ("6ca5ef2d", "feat(vscode): show the PR and Claude badges together, coloured by severity"),
    ("871a89ed", "test(sessions,cli): close the claude-wrap coverage gaps"),
    ("e4d2bec6", "docs(docs,release): add ADR-0057 and document the stream wrapper feed"),
    ("eed2ad79", "feat(vscode): show Claude session cues on worktree rows"),
    ("3350ceb5", "feat(sessions,cli): install the claude-wrap shim and VS Code setting"),
    ("7dee29e9", "feat(sessions,cli): wrap Claude to report its exact session state"),
    ("532b936d", "feat(sessions): add an authoritative stream-state session event"),
    ("7debebb0", "test(daemon,cli): harden merge-queue coverage of gates and audit logs"),
    ("ab227012", "test(lib,daemon,cli): cover merge-queue gh and socket paths"),
    ("c88bb2ff", "docs(docs,release): add ADR-0056 and document the merge-queue op"),
    ("dc580711", "feat(vscode): add Add to Merge Queue to the worktrees tree view"),
    ("a4947bf7", "feat(cli): add worktrees merge-queue subcommand"),
    ("3c65540c", "feat(daemon): add batch merge-queue op gated by server-side eligibility"),
    ("d6733a83", "feat(lib): add merge-queue eligibility resolve and enqueue mutation"),
    ("bb0d8272", "test(git,cli): cover worktree rebase classify/render/confirm branches"),
    ("a56d9452", "docs(docs): document worktrees rebase (ADR-0055, guide, changelog)"),
    ("f4fa97d5", "feat(cli): add `omni-dev worktrees rebase` subcommand"),
    ("ad5292f2", "feat(git): add worktree_rebase engine, fetch remote main once per repo"),
    ("901cd8f9", "test(daemon): cover orphaned-admin prune skip and lock branches"),
    ("1536c8c0", "chore(release): prepare release v0.38.0"),
    ("e45dccd9", "feat(cli): declarative coverage-diff ignore-list in repo config"),
    ("c2ea99d1", "test(cli): cover the stdin read logic in the close-confirm path"),
    ("465b1b06", "docs(cli): fix unresolved SafetyReport intra-doc link"),
    ("d2279964", "test(cli, daemon): close coverage gaps in the op-parity commands"),
    ("ba6743c0", "fix(cli): address review of the daemon op-parity commands"),
    ("336f019a", "docs(docs, release): document the worktrees/sessions op-parity commands"),
    ("1412aed5", "feat(cli, sessions): add typed daemon-op parity commands"),
    ("608e125d", "feat(daemon): add streaming subscription client for push ops"),
    ("0aa60e42", "refactor(daemon): extract close audit lines into testable helpers (#1364)"),
    ("a60e8e9e", "docs(docs): reconcile close audit note with logged fields (#1364)"),
    ("e74156a4", "feat(daemon): log window key and audit failed close phases (#1364)"),
    ("b4909a2d", "docs(docs): document close-op audit logging (#1364)"),
    ("2f7c3517", "feat(daemon): add audit logging to the worktree close op (#1364)"),
    ("2c67230d", "feat(daemon,cli)!: run a selectable subset of daemon services"),
    ("6a09994c", "test(cli): close coverage gaps in the git worktree wrappers"),
    ("66043917", "test(request-log): cover worktree kind-arg and as_str arms"),
    ("f467437b", "feat(cli): add logged git worktree wrapper subcommands"),
    ("8d615d86", "feat(request-log): add worktree record kind with context querying"),
    ("1fed9eaa", "docs(docs): document the silent emphasis+code split in the JFM spec"),
    ("2ffbd38d", "feat(atlassian): silently split strong+code runs in JFM→ADF conversion"),
    ("935b89be", "docs(release): record ADF schema 56.1.13 snapshot bump in changelog"),
    ("1aeebdde", "docs(docs): update schema version references to 56.1.13"),
    ("3e355de9", "docs(atlassian): update ADF schema snapshot to 56.1.13"),
    ("cfcc13ef", "chore(release): prepare release v0.37.0"),
    ("636f4a1f", "chore(release): prepare vscode extension release v0.6.0"),
    ("4ed5496b", "test(daemon): cover PR-cache persistence and poller degraded paths"),
    ("ffcc5f2a", "docs(daemon): fix unresolved intra-doc link to polling_prefs_path"),
    ("bdaf6dc2", "feat(daemon): optimize PR-badge poller to dramatically reduce GitHub API burn"),
    ("589f5967", "feat(request-log): implement ground-truth counting of GitHub API calls"),
    ("a5966d3d", "docs(docs): document per-repository PR polling and its 15-minute lease"),
    ("08ac3b13", "feat(vscode): add the per-repo PR-polling toggle to the Worktrees view"),
    ("27658975", "feat(daemon): add a per-repository PR-poll toggle with a 15-minute lease"),
    ("17f8df2d", "test(daemon): cover rate-limit tray_label and warn-arg branches"),
    ("9a1ab24d", "feat(daemon): implement GitHub API rate-limit monitor"),
    ("8c196b7f", "docs(docs): add Windows daemon port design (ADR-0054 and plan doc)"),
    ("41975a56", "docs(docs): finish repointing Windows-port references to #1363"),
    ("73ee5d66", "fix(vscode): keep the degraded gh fallback quiet for pr_none branches"),
    ("b6992ee0", "fix(daemon, docs): report an explicit \"no open PR\" negative on the tree"),
    ("813765e5", "feat(vscode): copy repository/worktree directory from the Worktrees view"),
    ("eb3c4f1f", "chore(workflows): bump actions/setup-node from 6 to 7"),
    ("328f6a81", "chore(workflows): bump EmbarkStudios/cargo-deny-action from 2.0.20 to 2.1.1"),
    ("c4287b09", "chore(cargo): bump dependencies in rust-minor-patch group"),
    ("52d1f267", "docs(docs): repoint Windows-port references from #1237 to #1363"),
    ("5d9a1b1d", "docs(docs): fix asciicast image format in README from SVG to PNG"),
    ("b4d8bcf0", "perf(vscode,daemon,docs): overlap multi-select close heartbeat waits"),
    ("8d12aa4b", "feat(vscode, docs): open a worktree's or repo's PR in a browser"),
    ("4e72173f", "docs(atlassian): update ADF schema snapshot to 56.1.3"),
    ("fbcbec11", "docs(docs): add comprehensive Bedrock model discovery guide"),
    ("0444dfa8", "feat(vscode): implement new session mode for Claude Code button"),
    ("c9f5bc91", "test(lib): close the retry_on_etxtbsy and preflight probe coverage gaps"),
    ("4a245a86", "docs(release): note the ETXTBSY shim-flake fix in the changelog"),
    ("8579cb82", "ci(workflows): disable fail-fast on the Test matrix"),
    ("7bb98da9", "test(lib): retry shim execs on ETXTBSY to close the fake-gh flake"),
    ("88694785", "fix(lib): surface the claude-cli preflight probe's spawn error"),
    ("6a404f45", "chore(release): prepare vscode extension release v0.5.0"),
    ("a6b183eb", "chore(release): prepare release v0.36.0"),
    ("01d68ded", "test(daemon): cover the worktree-removal error paths"),
    ("bc81401f", "fix(daemon): handle concurrent-writer race during worktree removal"),
    ("0b37fdad", "test(daemon): serialise the fake-gh shims against the ETXTBSY exec race"),
    ("66d86853", "test(daemon): cover the branchless worktree in the PR badge fold"),
    ("0ed97668", "fix(daemon, docs): re-ask GitHub when a commit moves a worktree's head"),
    ("1b63f460", "fix(daemon): invalidate a PR badge whose verdict is for another commit"),
    ("8d90b0b0", "fix(daemon): treat a PR whose suite is still creating jobs as pending"),
    ("10bf6082", "test(daemon): cover the PR badge degradation paths and drop unreachable arms"),
    ("de9049d6", "feat(daemon): resolve PR check badges in the daemon and fan them out"),
    ("127846b0", "feat(daemon, vscode): carry the worktree HEAD SHA on the tree snapshot"),
    ("826dc8e5", "docs(docs): add ADR-0052 and the sessions-service operator guide"),
    ("57c87114", "feat(vscode): report each window's Claude embeddings to the sessions service"),
    ("6864f75a", "feat(sessions,cli): add the sessions CLI and refresh the help snapshot"),
    ("d7df2b6a", "feat(sessions,daemon): host the Claude sessions tracker as a daemon service"),
    ("31485070", "feat(sessions): add cross-window Claude session registry and transcript watcher"),
    ("caa692bf", "chore(scopes): register the sessions commit scope"),
    ("ba09ad08", "test(atlassian): cover the last uncovered #1117 line"),
    ("49709e34", "test(atlassian,mcp): close the residual #1117 coverage gaps"),
    ("049d7bd2", "test(mcp): cover the jira_comment_delete tool handler"),
    ("c6782afb", "test(atlassian): cover the remaining #1117 execute() and dispatch glue"),
    ("b89eb488", "test(mcp): cover the new #1117 tool handlers end-to-end"),
    ("7668ca08", "test(atlassian): cover the new grab-bag CLI execute() paths"),
    ("7b248d58", "docs(docs): document the #1117 Atlassian mutation surface"),
    ("cf3b1218", "feat(atlassian,mcp): complete the Confluence write surface"),
    ("a0a4fc00", "fix(atlassian,docs): round-trip unsupported inline ADF nodes"),
    ("5b47a392", "docs(docs, release): document Snowflake multi-statement query support"),
    ("7d568bd9", "feat(cli, mcp, snowflake)!: return one result set per statement"),
    ("8a129073", "feat(snowflake): add multi-statement query support to the v1 client"),
    ("c0b097d5", "feat(snowflake): implement query cancellation without session eviction"),
    ("69cabad5", "chore(workflows): upgrade actions/upload-artifact to v7"),
    ("2cabbeb3", "chore(ci): bump dorny/paths-filter from 3 to 4"),
    ("2a674dda", "chore(ci): bump actions/setup-node from 4 to 6"),
    ("dee4d262", "feat(vscode): implement move Claude session command for worktree relocation"),
    ("a7f3379f", "chore(release, cargo): bump tokio-tungstenite from 0.29.0 to 0.30.0"),
    ("19ce1885", "chore(cargo): bump rust-minor-patch dependencies"),
    ("079d97a1", "docs(docs, release): document parent-folder workspace-trust workaround"),
    ("c1f69637", "docs(docs): add ADR-0051 on workspace trust for daemon-opened worktrees"),
    ("8d150f81", "feat(claude): make the HTTP AI request timeout configurable"),
    ("3d2929d4", "fix(claude): warn-and-ignore --beta-header on the OpenAI and Ollama backends"),
    ("58df2cbf", "feat(daemon): add CLI logs, per-service control, version handshake"),
    ("e6381604", "docs(vscode): document the Open Claude Code button"),
    ("cb955cce", "feat(vscode): add \"Open Claude Code\" editor title-bar button"),
    ("b07de46c", "refactor(vscode): remove unused omniDevWorktrees.open command"),
    ("3a9ae648", "docs(docs): note the colored worktree PR check badge"),
    ("d360bb14", "feat(vscode): color the worktree PR check badge via file decorations"),
    ("fe2bf02b", "feat(vscode): add install script for VS Code extension installation"),
    ("ffd592b9", "docs(docs): document the mcp settings.json configuration section"),
    ("eccae832", "feat(mcp): make the response truncation cap configurable"),
    ("831b58e2", "feat(mcp): seed the tracing filter from settings.mcp.log_level"),
    ("06896b71", "feat(mcp): fall back to settings.mcp.default_model in ai_chat"),
    ("5567b533", "feat(lib): add optional mcp section to settings.json schema"),
    ("f15d1a70", "docs(docs): add ADR-0050 and document worktree PR badges"),
    ("a7477e70", "feat(vscode): show PR badges on worktree rows"),
    ("a36d4bfc", "feat(vscode): resolve PR badges and CI checks via gh"),
    ("aeccaaa2", "feat(vscode): model PR badges in the worktrees tree"),
    ("58871cc3", "docs(docs,release): document worktrees refresh-interval env knobs"),
    ("4f46fec5", "perf(daemon): relax and env-tune worktrees refresh intervals"),
    ("6b67e43a", "docs(docs): document the daemon log sink and default log level"),
    ("bb803453", "feat(cli): default the daemon run process to info-level logging"),
    ("2d90b137", "feat(daemon): sink launchd daemon stdio to a 0600 daemon.log"),
    ("2d891798", "chore(release): prepare vscode extension release v0.4.0"),
    ("a24f1d76", "chore(release): prepare release v0.35.0"),
    ("32fd734b", "refactor(cli): improve ahead-behind merge robustness and add defensive tests"),
    ("867d2e3f", "docs(docs): document lazy per-worktree ahead/behind"),
    ("2277041f", "feat(vscode): fetch ahead/behind lazily on repo expand"),
    ("830f0ad6", "feat(cli): fetch ahead/behind on demand in worktrees tree"),
    ("b621837e", "feat(daemon): make per-worktree ahead/behind lazy via an on-demand op"),
    ("646f49f0", "test(daemon): update subscribe-snapshot assertions for show_closed"),
    ("8199a95d", "docs(docs): document the daemon-backed show/hide-closed toggle"),
    ("9f5f5148", "feat(vscode): drive the show/hide-closed toggle from the daemon"),
    ("adfaa4e3", "feat(daemon): back the worktrees show/hide-closed toggle with the daemon"),
    ("61df57cb", "fix(vscode): build the PR webview URI the GitHub extension accepts"),
    ("ad92f34b", "fix(vscode): resolve gh via well-known paths for GUI-launched editors"),
    ("5ef2470b", "docs(vscode): document the Open Pull Request action"),
    ("27d60d13", "feat(vscode): add Open Pull Request action to the Worktrees view"),
    ("690949fb", "feat(vscode): add gh-backed pull request discovery"),
    ("79f66122", "feat(vscode): mark github repos in the worktrees tree contextValue"),
    ("22513c37", "fix(ci, vscode): make the Marketplace publish optional and cut extension 0.3.0"),
    ("530546c2", "feat(vscode): hide closed worktrees in tree view with client-side toggle"),
    ("16bbfc83", "chore(release): prepare release v0.34.0"),
    ("12bf5b59", "fix(ci): exclude vscode-* tags from the crate release trigger"),
    ("d5279ae8", "fix(ci,vscode): make Open VSX publish optional in the extension release"),
    ("4baec3b0", "docs(vscode, workflows): rename extension from omni-dev Worktrees to omni-dev"),
    ("1c7a14d4", "docs(vscode): add VS Code extension changelog and release documentation"),
    ("697edca7", "docs(vscode): add gallery icon and packaging instructions"),
    ("44fa7f62", "feat(vscode, workflows): implement VS Code extension release pipeline"),
    ("313fb95f", "test(daemon): cover remaining close safety-check degradation paths"),
    ("e94e4aed", "chore(scopes): add vscode scope for the companion extension"),
    ("51bef249", "test(daemon): cover worktree close safety-check risk branches"),
    ("d5a6631c", "docs(docs): add ADR-0049 and document the worktrees close op"),
    ("1ca42f71", "feat(vscode): add Close Worktree/Close Window menus to the companion"),
    ("034c2e60", "feat(daemon): signal cross-window worktree close via heartbeat directive"),
    ("3bbaba68", "feat(daemon): add close op to worktrees service for worktree removal"),
    ("6c2bf2ba", "docs(docs): add ADR-0048 for worktrees tree view and push subscriptions"),
    ("e040f964", "feat(cli): add window key to worktree current badge identification"),
    ("bc9ef675", "feat: add VS Code worktrees tree view UI"),
    ("1066b95f", "feat(daemon): implement push subscriptions for worktrees service (#1267)"),
    ("98cc22e7", "feat(daemon): add open op to worktrees service for socket-driven code launches"),
    ("08d60584", "test(cli, daemon): close worktrees tree coverage gaps"),
    ("ee928ddd", "feat(cli, daemon): implement worktrees tree command for repository enumeration"),
    ("19cccb8b", "feat(snowflake,docs): thread browser-launch command from settings"),
    ("6fada292", "test(claude): cover AiClient cost-metadata send paths"),
    ("92d37a94", "feat(claude): add per-invocation USD cost metadata to AI responses"),
    ("f79c8eca", "docs(atlassian): update ADF schema snapshot to 56.1.1"),
    ("625b2bd2", "feat(claude): expand provider inference to recognize OpenAI and Gemini models"),
    ("7e2e1b1c", "chore(release): prepare release v0.33.0"),
    ("8f513fd0", "docs(docs): add ADR-0047 for remote-first fail-closed base resolution"),
    ("daf7492e", "feat(docs)!: add ADR-0046 unified output format convention"),
    ("03c66f55", "feat(snowflake,docs): add SNOWFLAKE_HOST override (PrivateLink/custom)"),
    ("1944bd0b", "docs(docs): add ADR-0045 for isolated named credential profiles"),
    ("b8ae4483", "feat(snowflake): expose disconnect --id / --all via socket and CLI"),
    ("025271ab", "docs(docs): add ADR-0044 for unified AI backend and model resolution"),
    ("e1359176", "test(daemon): add folderless window test case to worktrees menu items"),
    ("564ab55b", "feat(daemon): implement off-thread menu refresh for worktrees tray"),
    ("803238f1", "docs(docs): add ADR-0043 for default-on credential redaction"),
    ("b9e54e05", "docs(docs): document CI path-split required-check pattern (#598)"),
    ("bb6c23ed", "docs(docs): add ADR-0042 for the request-log subsystem"),
    ("837ae9b8", "docs(mcp): name the real jira_link tool variants in dry_run doc"),
    ("1233cda4", "chore(mcp): upgrade rmcp from 1.7.0 to 2.1.0"),
    ("ed883da4", "docs: clarify dry_run helper scope"),
    ("568604de", "ci: implement path-split gate to accelerate docs-only PRs"),
    ("5a808743", "docs: update issue references from #1041 to #1237"),
    ("604f8a54", "feat(snowflake): add CSV/TSV output formats and file writing capability"),
    ("0782bf5f", "docs(docs): add ADR-0041 for pushed commit amendment guard"),
    ("359868d3", "docs(docs): amend ADR-0027 to narrow MCP dry-run scope for create/write tools"),
    ("d69c9f46", "feat(snowflake): add non-interactive auth via PAT and key-pair JWT"),
    ("9106455a", "test(atlassian,cli,mcp): cover attachment handler/wrapper paths"),
    ("298da98a", "feat(atlassian,cli,mcp)!: implement JIRA attachment upload and delete operations"),
    ("7c4afdc3", "feat(workflows): ship VS Code worktrees companion extension (#1111)"),
    ("7346d964", "feat(snowflake): thread originating invocation id for request correlation"),
    ("8d5ab909", "feat(cli, mcp): add --ignore-filename-regex for coverage diff filtering"),
    ("43d187b2", "docs(docs): remove git worktree convention guidance"),
    ("7acb6559", "chore(release): prepare release v0.32.0"),
    ("4cf4f7cf", "test(daemon): unit-test the systemd systemctl runner logic"),
    ("689ef608", "test(daemon): cover systemd unit-file writing"),
    ("690fbca1", "test(daemon): cover systemd helper functions"),
    ("238fac6f", "feat(daemon): add Linux systemd socket activation for daemon auto-start at login"),
    ("fc5f6f16", "chore(scopes): register datadog commit scope"),
    ("f91e97ed", "fix(claude): estimate_lines_changed must not sum negative counts"),
    ("391cd21c", "refactor(atlassian,datadog): extract response handling helpers"),
    ("397ce700", "refactor(daemon,snowflake,browser,docs): migrate mod.rs to named files"),
    ("e3ec8508", "feat(cli, daemon): add git enrichment to worktrees service"),
    ("dcce8df8", "refactor(atlassian): split JIRA and Confluence DTOs into separate modules"),
    ("00cbbc74", "feat(browser)!: implement per-origin allowlist for bridge access control"),
    ("513d7889", "test(cli): close transcript fetch coverage gaps"),
    ("7e2cbcab", "feat(cli): implement auto-detecting transcript fetch subcommand"),
    ("9bde26ba", "refactor(daemon, lib): split worktrees engine from service adapter"),
    ("ed103c84", "feat(browser): add binary request body support with base64 encoding"),
    ("40935e28", "refactor(lib): drop unreachable retry_429 tail to close coverage gap"),
    ("1eb00be5", "test(request-log): cover prune and rotation paths flagged by coverage"),
    ("30c1e650", "feat(request-log): implement log rotation and prune command"),
    ("c84534bd", "fix(daemon): close extra file descriptors from launchd socket activation"),
    ("6c70cddc", "fix(request-log): redact secret-bearing URLs in invocation argv"),
    ("0bc82a70", "feat(claude): add escape hatch source provenance to sandbox-weakened warnings"),
    ("2e6638d2", "docs(browser): note stdout token exposure as accepted risk (#1148)"),
    ("b6f9880d", "feat(claude, cli): implement unified AI backend and model selection"),
    ("84acb778", "feat(claude): centralize AI backend and model resolution"),
    ("c672a088", "docs(docs,snowflake): sweep documentation drift from the gap analysis"),
    ("e206e75e", "feat(claude)!: scrub secret env vars when tool-access escape hatch is enabled"),
    ("aec9db35", "docs(docs): correct SO_REUSEADDR fail-closed binding prose"),
    ("5e834a0f", "docs(docs): document prompt-body exposure in DEBUG traces and errors"),
    ("615af5d6", "test(atlassian): close attachment-delete coverage gaps flagged on #1175"),
    ("2b53c099", "test(atlassian,datadog,cli): cover profile-write gaps flagged on #1177"),
    ("9b7f813f", "feat(atlassian): write auth credentials to active profile instead of base env"),
    ("1824d6fc", "fix(snowflake): resolve profile-scoped defaults client-side"),
    ("5409eaf7", "fix(daemon): detach the non-macOS daemon start spawn for real"),
    ("cd3318e4", "fix(snowflake): run keep-alive heartbeat so idle pools skip re-SSO"),
    ("efc7e511", "chore(claude): add claude build and analysis command shortcuts"),
    ("d269d458", "feat(atlassian): support comma-separated values for array-typed JIRA fields"),
    ("16bb7e7c", "fix(cli): sanitize terminal control bytes in worktrees list output"),
    ("7c8c701d", "test(lib,atlassian): cover settings env-helper gaps flagged on #1163"),
    ("948ff638", "fix(atlassian): bound JFM→ADF nesting depth to prevent stack overflow"),
    ("86655b6a", "fix(daemon,request-log): create runtime paths with tight modes"),
    ("4a5f6c87", "fix(claude): improve budget cap diagnostics and document limitations"),
    ("8457c06c", "fix(daemon): cap the worktrees registry at 256 entries"),
    ("f951d516", "fix(browser): avoid char-boundary panics in Facebook harvester windows"),
    ("b590e551", "fix(atlassian): redact credential secrets in Debug output"),
    ("1ff8eb29", "fix(request-log): redact secret-bearing URL query and fragment values"),
    ("3964b701", "feat(atlassian, mcp): add jira_edit tool and fix custom-field coercion"),
    ("1eecca22", "fix(atlassian): sanitize remote attachment filenames on download"),
    ("f56be74a", "refactor(atlassian): move attachment_filename to utils::path"),
    ("8a356ff9", "fix(request-log): scrub secret-bearing argv values before logging"),
    ("d3d807cf", "fix(request-log,docs): redact secret-bearing headers by name substring"),
    ("3f605c82", "test(atlassian): exercise every branch of the FailingWriter helper"),
    ("ce654d85", "test(atlassian): cover IO-error and applied-guard branches on #1099"),
    ("69b1ac10", "test(atlassian): close inline-comment coverage gaps flagged on #1099"),
    ("cded2185", "feat(atlassian): implement inline-comment drift auditing and re-anchoring"),
    ("64d739ff", "test(atlassian,mcp): cover account-id user-get lookup paths"),
    ("b180c4c2", "feat(atlassian): add user ID lookup (account ID → record resolution)"),
    ("9acd2b96", "test(mcp): close content_path coverage gaps flagged on #1098"),
    ("6f02e05e", "test(mcp): add backstop test for MCP tool descriptions and parameter schemas"),
    ("6d7e8d60", "chore(release, cargo): bump quick-xml from 0.40.1 to 0.41.0"),
    ("a573cfa5", "test(atlassian): cover source-location helper branches"),
    ("656d287a", "fix(mcp): drop now-unused AdfDocument import"),
    ("bef48ae3", "chore(release): prepare release v0.31.0"),
    ("2341f99a", "refactor(claude): inject env into discovery seams to close #821 race"),
    ("8b679d04", "docs(docs): sync mcp.md catalog with the audited surface"),
    ("5ada3dbc", "docs(cli): cross-reference MCP tools from subcommand help"),
    ("b65660c7", "docs(mcp): audit tool and parameter descriptions for AI-agent clarity"),
    ("ec6fa1cc", "docs(docs): add STYLE-0029 MCP description checklist"),
    // Moved from FAILING_FIXTURES: `coverage` became a real scope in #1468.
    ("92bfe968", "refactor(atlassian,cli,coverage): migrate remaining mod.rs files"),
];

/// Real commit subjects from the pinned corpus range with a known,
/// justified rule violation. `(short_hash, subject, expected_rule)` —
/// see the module doc for why each category is a real historical
/// exception, not a lint bug.
#[rustfmt::skip]
const FAILING_FIXTURES: &[(&str, &str, &str)] = &[
    ("7bb7bcac", "Add MCP Toplist rank badge", "format"),
    ("f0572348", "feat(daemon): resolve orphaned worktree admin metadata after out-of-band deletion", "subject-length"),
    ("d4786e2c", "feat(daemon): reduce GitHub API rate-limit burn by ~70% through PR-poll budget folding and idle-gate rate poller", "subject-length"),
    ("b3f8d1fa", "feat(daemon, vscode): move pull-request lookups to daemon with shared, TTL-cached op", "subject-length"),
    ("d20f5194", "feat(daemon): capture git commit provenance at build time and surface in daemon status", "subject-length"),
    ("3e96f2de", "feat(vscode): implement multi-select on Worktrees view with separate window and delete operations", "subject-length"),
    ("fed09b14", "fix(daemon, vscode, docs): re-render the tree when a push moves a worktree's upstream", "subject-length"),
    ("bee3e922", "feat(vscode, daemon, docs): consume daemon PR badges and retire the extension reducer", "subject-length"),
    ("7d255d77", "feat(atlassian,cli,mcp): complete the JIRA write surface + auth logout + global --instance", "subject-length"),
    ("87a496aa", "fix(claude, cli): improve AI error handling and prevent template clobbering on failures", "subject-length"),
    ("6e4bbf74", "feat(claude, cli): refresh model registry to current Claude lineup and add retirement dates", "subject-length"),
    ("9d1cb513", "docs(docs): document AI backend structured-output, beta-header, and timeout parity", "subject-length"),
    ("cbce5120", "feat(claude): structured JSON-schema output on the Anthropic and Bedrock backends", "subject-length"),
    ("42b01029", "perf(daemon): coalesce per-window tree computation into shared single-flight cache", "subject-length"),
    ("795db43e", "feat(atlassian): implement transition-screen field resolution and comment routing", "subject-length"),
    ("bd8c0dfd", "feat(cli, daemon): add parent repository tracking and worktree distinction in tray and list", "subject-length"),
    ("4099feea", "docs(docs): formalize service engine/adapter split architecture in ADRs 0039 and 0040", "subject-length"),
    ("26bfb45d", "chore(deps): bump the rust-minor-patch group with 2 updates", "unknown-scope"),
    ("46915a16", "feat(mcp,git,data,atlassian): expose newer-subsystem tools and add jira_search filters", "subject-length"),
    ("ce9d282e", "feat(cli)!: unify machine-readable output selection on `-o`/`--output`; reserve `--out-file` for file destinations", "subject-length"),
    ("718bda44", "feat(request-log): close log coverage gaps — transcript HTTP recording, AI backend service tags, numeric query fields, and absolute time bounds", "subject-length"),
    ("ef47479b", "refactor(claude, atlassian, request-log): consolidate HTTP 429 retry logic into shared driver", "subject-length"),
    ("80b2df17", "feat(claude, cli): make --model, --beta-header global; extend --ai-backend to openai, ollama, bedrock", "subject-length"),
    ("be0769c7", "feat(cli,atlassian)!: add --dry-run to confluence attachment delete and migrate to shared guard", "subject-length"),
    ("bd3bfa96", "feat(git,cli)!: populate in_main_branches field and guard amend against pushed commits", "subject-length"),
    ("44459e6c", "feat(cli,git)!: unify default base-branch resolution and prefer remote-tracking refs", "subject-length"),
    ("19d52854", "fix(settings): write credential store 0600/0700 via shared env helpers", "unknown-scope"),
    ("c35d3927", "feat(mcp)!: add `*_path` parameters to write-side tools for filesystem content input", "subject-length"),
    ("c85f9b54", "feat(atlassian, snowflake): implement credential profiles for multi-tenant support", "subject-length"),
    ("02655b48", "docs(atlassian): add preservation guidance for localId attributes in Confluence operations", "subject-length"),
    ("8f4781e2", "feat(atlassian): enrich ADF validation errors with source location and offending text", "subject-length"),
];

/// The one real historical commit needing a full multi-line message (not
/// just a subject) to exercise the blank-line rule authentically:
/// `6413fd73`, ci(workflows): fix CI race condition in release tag
/// filtering — a clean 61-char subject with git-folded no-blank-line-2
/// body, over 400 commits the only one so shaped.
const BLANK_LINE_FIXTURE_HASH: &str = "6413fd73";
const BLANK_LINE_FIXTURE_MESSAGE: &str = "ci(workflows): fix CI race condition in release tag filtering\nExcludes vscode extension tags from crate release CI to prevent Coverage job 404s when tag-push CI runs concurrently with binary upload.\nThe bare `v*` tag filter matches both crate release tags (`v1.0.0`) and extension tags (`vscode-v1.0.0`), causing the crate's full CI suite to run on extension releases. The Coverage job then 404s trying to fetch the not-yet-uploaded binary from the in-flight crate release.\n- Add `!vscode-*` exclusion to tag filter (mirrors release.yml fix #1288) - Skip Coverage job on tag pushes since validation already occurred on main (Coverage is only meaningful for PRs and main pushes)\nFixes #1289";

/// This repo's real `.omni-dev/scopes.yaml`, loaded once so the corpus is
/// checked against the actual project scope list rather than an empty or
/// synthetic one — the same source `omni-dev git commit message lint`
/// itself would resolve.
fn real_scopes() -> Vec<omni_dev::data::context::ScopeDefinition> {
    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let context_dir = omni_dev::claude::context::resolve_context_dir_at(None, &repo_root);
    omni_dev::claude::context::load_project_scopes(&context_dir, &repo_root)
}

#[test]
fn passing_corpus_lints_cleanly() {
    let rules = CommitRules::default();
    let valid_scopes = real_scopes();

    let mut unexpected_failures = Vec::new();
    for (hash, subject) in PASSING_SUBJECTS {
        let issues = lint_message(subject, &rules, &valid_scopes);
        if !omni_dev::git::lint_passes(&issues) {
            let rules: Vec<_> = issues
                .iter()
                .filter(|i| i.severity == omni_dev::data::check::IssueSeverity::Error)
                .map(|i| i.rule.as_str())
                .collect();
            unexpected_failures.push(format!("{hash} {rules:?} {subject:?}"));
        }
    }

    assert!(
        unexpected_failures.is_empty(),
        "commit(s) expected to pass cleanly were flagged \
         (scopes.yaml, commit-rules.yaml default, or a rule implementation \
         changed since this fixture was captured — investigate before \
         moving an entry to FAILING_FIXTURES):\n{}",
        unexpected_failures.join("\n")
    );
}

#[test]
fn failing_corpus_flags_exactly_its_known_rule() {
    let rules = CommitRules::default();
    let valid_scopes = real_scopes();

    let mut mismatches = Vec::new();
    for (hash, subject, expected_rule) in FAILING_FIXTURES {
        let issues = lint_message(subject, &rules, &valid_scopes);
        let error_rules: Vec<&str> = issues
            .iter()
            .filter(|i| i.severity == omni_dev::data::check::IssueSeverity::Error)
            .map(|i| i.rule.as_str())
            .collect();
        if !error_rules.contains(expected_rule) {
            mismatches.push(format!(
                "{hash} {subject:?}: expected error rule {expected_rule:?}, got {error_rules:?}"
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "known-failing corpus entries no longer flag their expected rule:\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn blank_line_fixture_flags_exactly_its_known_rule() {
    let rules = CommitRules::default();
    let valid_scopes = real_scopes();

    let issues = lint_message(BLANK_LINE_FIXTURE_MESSAGE, &rules, &valid_scopes);
    let error_rules: Vec<&str> = issues
        .iter()
        .filter(|i| i.severity == omni_dev::data::check::IssueSeverity::Error)
        .map(|i| i.rule.as_str())
        .collect();
    assert!(
        error_rules.contains(&"blank-line-after-subject"),
        "{BLANK_LINE_FIXTURE_HASH} should flag blank-line-after-subject, got {error_rules:?}"
    );
}

/// The corpus itself must be the size it claims — catches an accidental
/// truncation of the fixture arrays above.
#[test]
fn corpus_size_matches_the_pinned_range() {
    assert_eq!(PASSING_SUBJECTS.len(), 367);
    assert_eq!(FAILING_FIXTURES.len(), 31);
    // 367 passing + 31 failing + 1 blank-line special case = 399, the full
    // non-merge commit count of the pinned a28f9598..466cf0fc range.
    assert_eq!(PASSING_SUBJECTS.len() + FAILING_FIXTURES.len() + 1, 399);
}
