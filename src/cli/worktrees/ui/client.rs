//! Typed one-shot wrapper over [`DaemonClient`] for the ops the worktrees UI
//! calls directly, outside the two subscribed feeds ([`super::supervisor`]
//! drives `tree`/`subscribe` and `sessions`/`list`/`subscribe` generically).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::wire::{AheadBehindEntryWire, SafetyReportWire};
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
}
