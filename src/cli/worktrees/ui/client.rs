//! Typed one-shot wrapper over [`DaemonClient`] for the ops the worktrees UI
//! calls directly, outside the two subscribed feeds ([`super::supervisor`]
//! drives `tree`/`subscribe` and `sessions`/`list`/`subscribe` generically).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::json;

use super::wire::AheadBehindEntryWire;
use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::DaemonEnvelope;

const SERVICE: &str = "worktrees";

/// A thin, typed client over the daemon's `worktrees` service ops.
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
        let reply = self
            .inner
            .request(DaemonEnvelope::service(SERVICE, "ahead-behind", payload))
            .await?;
        if !reply.ok {
            bail!(
                "daemon returned an error: {}",
                reply.error.as_deref().unwrap_or("unknown error")
            );
        }
        let raw: HashMap<String, AheadBehindEntryWire> = reply
            .payload
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
}
