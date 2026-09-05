//! Typed one-shot wrapper over [`DaemonClient`] for the ops the worktrees UI
//! calls directly, outside the two subscribed feeds ([`super::supervisor`]
//! drives `tree`/`subscribe` and `sessions`/`list`/`subscribe` generically).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::wire::{
    AheadBehindEntryWire, MergeQueueReplyWire, PushReplyWire, RebaseReplyWire, SafetyReportWire,
};
use crate::cli::daemon::call_service;
use crate::daemon::client::DaemonClient;

const SERVICE: &str = "worktrees";

/// A thin, typed client over the daemon's `worktrees` service ops. Every op
/// routes through [`call_service`] (the shared request/bail/origin-stamping
/// helper `src/cli/daemon.rs`'s own `daemon bridge`/`daemon service` clients
/// use) rather than hand-rolling per-op request/reply plumbing.
#[derive(Debug, Clone)]
pub struct WorktreesClient {
    inner: DaemonClient,
}

impl WorktreesClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            inner: DaemonClient::new(socket),
        }
    }

    fn socket(&self) -> &Path {
        self.inner.socket_path()
    }

    /// Batches the daemon's lazy `ahead-behind` op over `paths`.
    ///
    /// A path the daemon has no upstream/divergence to report for is omitted
    /// from the returned map entirely — the daemon reports absence, not zero
    /// (`src/daemon/services/worktrees.rs::ahead_behind_results`) — so callers
    /// must treat a missing key as "unavailable", never as `0`/`0`.
    pub async fn fetch_ahead_behind(
        &self,
        paths: &[PathBuf],
    ) -> Result<HashMap<PathBuf, AheadBehindEntryWire>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let payload = json!({ "paths": paths });
        let value = call_service(self.socket(), SERVICE, "ahead-behind", payload).await?;
        let raw: HashMap<String, AheadBehindEntryWire> = value
            .get("results")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        Ok(raw
            .into_iter()
            .map(|(k, v)| (PathBuf::from(k), v))
            .collect())
    }

    /// Focuses `path`'s VS Code window via the daemon's `open` op (the same
    /// op `omni-dev worktrees focus` wraps, `src/cli/worktrees.rs`). `path`
    /// must already be an absolute, existing directory — this method does no
    /// canonicalization of its own.
    pub async fn open(&self, path: &Path) -> Result<()> {
        call_service(self.socket(), SERVICE, "open", json!({ "path": path })).await?;
        Ok(())
    }

    /// The `close` op's phase-1 safety check (`remove: true`, unconfirmed) —
    /// side-effect-free; only reports whether/how `path` could be removed.
    pub async fn close_check(&self, path: &Path) -> Result<SafetyReportWire> {
        let value = call_service(
            self.socket(),
            SERVICE,
            "close",
            json!({ "path": path, "remove": true }),
        )
        .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// The `close` op's phase-2 execute: `remove: true, confirmed: true`
    /// deletes a removable linked worktree; `remove: false` just closes the
    /// owning window(s) without deleting anything (the "Close Window" action
    /// — non-destructive, no phase-1 check needed).
    pub async fn close_execute(&self, path: &Path, remove: bool) -> Result<()> {
        let payload = if remove {
            json!({ "path": path, "remove": true, "confirmed": true })
        } else {
            json!({ "path": path, "remove": false })
        };
        call_service(self.socket(), SERVICE, "close", payload).await?;
        Ok(())
    }

    /// The `rebase` op (ADR-0059). Batched over `paths` and two-phase:
    /// `check` plans (fetch + classify, no history rewritten), `!check`
    /// executes after re-validating from scratch.
    ///
    /// `keep_conflicts` is sent `true` to match the VS Code tree view:
    /// leaving a conflicting worktree mid-rebase, to be resolved in place,
    /// is the point of #1415. Note this drives the **daemon's** op, never
    /// `worktrees rebase`'s local engine — ADR-0072 §9.
    pub async fn rebase(&self, paths: &[PathBuf], check: bool) -> Result<RebaseReplyWire> {
        let value = call_service(
            self.socket(),
            SERVICE,
            "rebase",
            json!({
                "paths": paths,
                "check": check,
                "confirmed": !check,
                "keep_conflicts": true,
            }),
        )
        .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// The `push` op (ADR-0061). Two-phase like [`rebase`](Self::rebase),
    /// though its plan phase contacts no remote at all — it classifies
    /// against the local remote-tracking ref, which is exactly what the
    /// lease is checked against, so the plan the user confirms and what
    /// `git` enforces agree by construction.
    ///
    /// **There is no force option**, here or anywhere in this crate's
    /// surface: every force the daemon issues is
    /// `--force-with-lease --force-if-includes`, so it cannot overwrite a
    /// remote tip it has not seen, and it never force-pushes the default
    /// branch. A refused lease is the feature working.
    pub async fn push(&self, paths: &[PathBuf], check: bool) -> Result<PushReplyWire> {
        let value = call_service(
            self.socket(),
            SERVICE,
            "push",
            json!({ "paths": paths, "check": check, "confirmed": !check }),
        )
        .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// The `merge-queue` op (ADR-0056). Two-phase: `check` reports
    /// eligibility only, `!check` enqueues the re-validated PRs through the
    /// user's own `gh`.
    pub async fn merge_queue(&self, paths: &[PathBuf], check: bool) -> Result<MergeQueueReplyWire> {
        let value = call_service(
            self.socket(),
            SERVICE,
            "merge-queue",
            json!({ "paths": paths, "check": check, "confirmed": !check }),
        )
        .await?;
        Ok(serde_json::from_value(value)?)
    }
}
