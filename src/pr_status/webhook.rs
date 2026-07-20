//! Reconstructs PR badges from buffered GitHub **webhook deltas**.
//!
//! Today's poller resolves each `(owner, name, branch)` target with one aliased
//! GraphQL query that hands back a *pre-rolled* `statusCheckRollup`. The #1378
//! webhook buffer instead delivers **per-check deltas** — one `check_run` per run,
//! one `check_suite` per app-suite, one `status` per legacy context — plus
//! `pull_request` events carrying the PR metadata the CI events lack (`isDraft`
//! and the browser `html_url`). This module aggregates those deltas, keyed by
//! `(owner, name, branch, head_sha)`, and reduces them with the **same**
//! [`super::rollup_check_state`] the GraphQL path uses (each delta normalised into
//! the rollup-node shape that reducer already understands) — so a webhook verdict
//! is the verdict the poll would have produced. Proven against 80 live events in
//! the #1378 spike (`deploy/webhook-buffer/verify-fields.mjs`); the check-identity
//! scheme (`run:` / `suite:` / `ctx:`) and the failure-dominates reduction match it.
//!
//! It is a **pure** aggregator — no daemon, socket, or network types. The daemon's
//! `WebhookPrSource` drives it: feed each `/events` pull's envelopes to
//! [`WebhookAggregator::ingest`], then [`WebhookAggregator::resolve`] against the
//! currently watched targets. `head_oid` is carried onto every badge so
//! [`PrBadge::is_stale_for`](super::PrBadge::is_stale_for) still invalidates on a
//! push with no network call, exactly as the poller's badges do.
//!
//! **Metadata split (confirmed by the spike):** CI events carry no `isDraft` and
//! only the REST `api` url, so a target with a reconstructed verdict but no
//! `pull_request` event yet is deliberately **left absent** here — the reconcile
//! poll mints its full badge. Once a `pull_request` event is seen, its metadata is
//! retained across pulls, so subsequent CI-only deltas mint real-time badges.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::{rollup_check_state, PrBadge, PrCheckState, PrResolution, PrTarget};

/// One buffered delivery from the webhook buffer's `GET /events` response: the
/// GitHub event name, its delivery id, the arrival timestamp (ms since epoch,
/// assigned by the Worker), and the raw webhook payload. Mirrors the envelope the
/// capture Worker stores (`deploy/webhook-buffer/worker.js`).
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct WebhookEvent {
    /// The `X-GitHub-Event` type (`check_run`, `check_suite`, `status`,
    /// `pull_request`, `ping`, …).
    pub event: String,
    /// Arrival time in epoch-ms, used to order deltas (latest state per check
    /// identity wins). Absent/`0` sorts oldest.
    #[serde(default)]
    pub received: u64,
    /// The raw GitHub webhook payload.
    #[serde(default)]
    pub payload: Value,
}

impl WebhookEvent {
    /// The `(owner, name)` this delivery is for, from its `repository` object, or
    /// `None` for an event without one (a malformed/unrelated payload). Lets the
    /// source record per-repo webhook activity for `daemon webhook status` without
    /// re-deriving the event shape.
    pub fn repo(&self) -> Option<(String, String)> {
        repo_of(&self.payload)
    }

    /// Extracts the `events` array from a `/events` reply body (`{ events: [...],
    /// cursor, ... }`), skipping anything that fails to deserialise. Unknown or
    /// malformed entries are dropped rather than failing the whole pull — a single
    /// bad delivery must never stall the source.
    pub fn from_reply(reply: &Value) -> Vec<Self> {
        reply
            .get("events")
            .and_then(Value::as_array)
            .map(|events| {
                events
                    .iter()
                    .filter_map(|e| serde_json::from_value(e.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// PR-level metadata for one branch, sourced **only** from `pull_request` events
/// (the CI events carry neither the draft flag nor the browser url).
#[derive(Debug, Clone)]
struct PrMeta {
    number: u64,
    is_draft: bool,
    /// The browser `html_url` — the badge's open action needs this, not the REST
    /// `api` url the CI events carry.
    url: String,
    /// Arrival time of the sourcing event, so a newer `pull_request` event wins.
    received: u64,
}

/// The classified deltas for one `(target, head_sha)` commit: the latest node seen
/// per check identity, plus that node's arrival time so an out-of-order older
/// delta cannot overwrite a newer state.
#[derive(Debug, Default)]
struct CommitChecks {
    /// `identity` (`run:<name|id>` / `suite:<app|id>` / `ctx:<context>`) → (arrival
    /// ms, normalised rollup node). The node is `{status, conclusion}` for a
    /// run/suite or `{state}` for a legacy context — the shape
    /// [`super::check_entry_state`] classifies.
    entries: HashMap<String, (u64, Value)>,
}

/// What a single webhook event contributes to the aggregate.
struct Extracted {
    target: PrTarget,
    sha: String,
    /// `(identity, normalised node)` when the event carries a check verdict.
    check: Option<(String, Value)>,
    /// PR metadata when the event is a `pull_request` event.
    pr: Option<PrMeta>,
}

/// Aggregates webhook deltas into `(target, head_sha)` rollups and per-branch PR
/// metadata, then reconstructs [`PrResolution`]s for the watched targets. Cheap to
/// construct; lives for the lifetime of the `WebhookPrSource` loop.
#[derive(Debug, Default)]
pub(crate) struct WebhookAggregator {
    /// Per `(target, head_sha)`: the latest classified node per check identity.
    checks: HashMap<(PrTarget, String), CommitChecks>,
    /// Per target: the most recent `pull_request` metadata (persists across pulls).
    pr_meta: HashMap<PrTarget, PrMeta>,
    /// Per target: the head sha of the most-recently-seen event and its arrival
    /// time — the commit `resolve` reconstructs the verdict for.
    head: HashMap<PrTarget, (String, u64)>,
}

impl WebhookAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one delivery into the aggregate. Non-CI/PR events (`ping`, unrelated
    /// types, or a payload missing repo/branch/sha) are ignored.
    pub fn ingest(&mut self, event: &WebhookEvent) {
        let Some(extracted) = extract(event) else {
            return;
        };
        let key = (extracted.target.clone(), extracted.sha.clone());

        if let Some((identity, node)) = extracted.check {
            let commit = self.checks.entry(key).or_default();
            match commit.entries.get(&identity) {
                // Latest state per identity wins; ties (same ms) keep the newer
                // arrival so a re-delivered terminal state is not lost.
                Some((seen, _)) if *seen > event.received => {}
                _ => {
                    commit.entries.insert(identity, (event.received, node));
                }
            }
        }

        if let Some(pr) = extracted.pr {
            match self.pr_meta.get(&extracted.target) {
                Some(existing) if existing.received > pr.received => {}
                _ => {
                    self.pr_meta.insert(extracted.target.clone(), pr);
                }
            }
        }

        // The current head is the sha of the most recently received event for the
        // branch. A push that moves the head is picked up here; `resolve` then reads
        // only that commit's checks, and `is_stale_for` guards the worktree side.
        match self.head.get(&extracted.target) {
            Some((_, seen)) if *seen > event.received => {}
            _ => {
                self.head
                    .insert(extracted.target, (extracted.sha, event.received));
            }
        }
    }

    /// Reconstructs a badge per watched target that has both a head commit and
    /// `pull_request` metadata. Prunes bookkeeping to the watch set and to each
    /// target's current head commit, bounding memory to what the tree shows.
    ///
    /// Targets with a verdict but no PR metadata yet, and targets with no data at
    /// all, are **omitted** (not `NoPr`) — a webhook stream can never assert "no
    /// open PR", so their resolution is left to the reconcile poll.
    pub fn resolve(&mut self, targets: &[PrTarget]) -> HashMap<PrTarget, PrResolution> {
        self.prune_to(targets);

        let mut out = HashMap::new();
        for target in targets {
            let Some((head_sha, _)) = self.head.get(target) else {
                continue; // no CI/PR activity buffered — reconcile decides.
            };
            let Some(meta) = self.pr_meta.get(target) else {
                continue; // verdict may exist, but no browser url/draft yet.
            };

            let checks = match self.checks.get(&(target.clone(), head_sha.clone())) {
                Some(commit) => {
                    let nodes: Vec<Value> = commit
                        .entries
                        .values()
                        .map(|(_, node)| node.clone())
                        .collect();
                    rollup_check_state(&nodes)
                }
                None => PrCheckState::None, // a PR with no checks reported yet.
            };

            out.insert(
                target.clone(),
                PrResolution::Pr(PrBadge {
                    number: meta.number,
                    is_draft: meta.is_draft,
                    checks,
                    url: meta.url.clone(),
                    head_oid: head_sha.clone(),
                }),
            );
        }
        out
    }

    /// Drops bookkeeping for targets no longer watched and, per remaining target,
    /// commit rollups for any sha other than the current head.
    fn prune_to(&mut self, targets: &[PrTarget]) {
        let watch: HashSet<&PrTarget> = targets.iter().collect();
        self.head.retain(|t, _| watch.contains(t));
        self.pr_meta.retain(|t, _| watch.contains(t));

        let head_shas: HashMap<PrTarget, String> = self
            .head
            .iter()
            .map(|(t, (sha, _))| (t.clone(), sha.clone()))
            .collect();
        self.checks
            .retain(|(t, sha), _| head_shas.get(t).is_some_and(|head| head == sha));
    }
}

/// Repo `(owner, name)` from any event's `repository` object.
fn repo_of(payload: &Value) -> Option<(String, String)> {
    let repo = payload.get("repository")?;
    let owner = repo.get("owner")?.get("login")?.as_str()?;
    let name = repo.get("name")?.as_str()?;
    Some((owner.to_string(), name.to_string()))
}

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = value;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
}

/// Normalises one webhook event into the fields the aggregate needs, or `None` if
/// it carries no `(repo, branch, head_sha)` triple (e.g. `ping`, or a fork check
/// event with a null `head_branch`).
fn extract(event: &WebhookEvent) -> Option<Extracted> {
    let payload = &event.payload;
    let (owner, name) = repo_of(payload)?;

    match event.event.as_str() {
        "check_run" => {
            let cr = payload.get("check_run")?;
            let branch = str_at(cr, &["check_suite", "head_branch"])?;
            let sha = cr.get("head_sha")?.as_str()?;
            // A CheckRun's identity is its name (stable across queued→completed);
            // fall back to its numeric id if unnamed.
            let identity = cr
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| cr.get("id").map(ToString::to_string))?;
            let node = json!({
                "status": cr.get("status").cloned().unwrap_or(Value::Null),
                "conclusion": cr.get("conclusion").cloned().unwrap_or(Value::Null),
            });
            Some(Extracted {
                target: PrTarget {
                    owner,
                    name,
                    branch: branch.to_string(),
                },
                sha: sha.to_string(),
                check: Some((format!("run:{identity}"), node)),
                pr: None,
            })
        }
        "check_suite" => {
            let cs = payload.get("check_suite")?;
            let branch = cs.get("head_branch")?.as_str()?;
            let sha = cs.get("head_sha")?.as_str()?;
            // A suite is one app's aggregate; key on the app id so re-deliveries
            // for the same app collapse, falling back to the suite id.
            let identity = cs
                .get("app")
                .and_then(|app| app.get("id"))
                .map(ToString::to_string)
                .or_else(|| cs.get("id").map(ToString::to_string))?;
            let node = json!({
                "status": cs.get("status").cloned().unwrap_or(Value::Null),
                "conclusion": cs.get("conclusion").cloned().unwrap_or(Value::Null),
            });
            Some(Extracted {
                target: PrTarget {
                    owner,
                    name,
                    branch: branch.to_string(),
                },
                sha: sha.to_string(),
                check: Some((format!("suite:{identity}"), node)),
                pr: None,
            })
        }
        "status" => {
            // `branches[]` lists only refs whose head is this sha; take the first.
            let branch = payload
                .get("branches")?
                .as_array()?
                .first()?
                .get("name")?
                .as_str()?;
            let sha = payload.get("sha")?.as_str()?;
            let context = payload.get("context")?.as_str()?;
            // A legacy StatusContext classifies on `state` alone (no COMPLETED gate).
            let node = json!({ "state": payload.get("state").cloned().unwrap_or(Value::Null) });
            Some(Extracted {
                target: PrTarget {
                    owner,
                    name,
                    branch: branch.to_string(),
                },
                sha: sha.to_string(),
                check: Some((format!("ctx:{context}"), node)),
                pr: None,
            })
        }
        "pull_request" => {
            let pr = payload.get("pull_request")?;
            let branch = str_at(pr, &["head", "ref"])?;
            let sha = str_at(pr, &["head", "sha"])?;
            let number = payload
                .get("number")
                .and_then(Value::as_u64)
                .or_else(|| pr.get("number").and_then(Value::as_u64))?;
            let is_draft = pr.get("draft").and_then(Value::as_bool).unwrap_or(false);
            let url = pr.get("html_url")?.as_str()?.to_string();
            Some(Extracted {
                target: PrTarget {
                    owner,
                    name,
                    branch: branch.to_string(),
                },
                sha: sha.to_string(),
                check: None,
                pr: Some(PrMeta {
                    number,
                    is_draft,
                    url,
                    received: event.received,
                }),
            })
        }
        _ => None, // ping / unrelated event.
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const OWNER: &str = "acme";
    const NAME: &str = "widget";
    const BRANCH: &str = "feature/spike";
    const SHA: &str = "9a7c1f0e2b3d4c5f60718293a4b5c6d7e8f90112";

    fn target() -> PrTarget {
        PrTarget {
            owner: OWNER.into(),
            name: NAME.into(),
            branch: BRANCH.into(),
        }
    }

    /// Loads a `deploy/webhook-buffer/samples/*.json` fixture (a single envelope or
    /// an array of them) into events.
    fn sample(file: &str) -> Vec<WebhookEvent> {
        let path = format!(
            "{}/deploy/webhook-buffer/samples/{file}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let value: Value = serde_json::from_str(&raw).expect("valid sample JSON");
        match value {
            Value::Array(items) => items
                .into_iter()
                .map(|v| serde_json::from_value(v).expect("envelope"))
                .collect(),
            other => vec![serde_json::from_value(other).expect("envelope")],
        }
    }

    fn all_samples() -> Vec<WebhookEvent> {
        let mut events = Vec::new();
        for file in [
            "check_run.json",
            "check_suite.json",
            "status.json",
            "pull_request.json",
        ] {
            events.extend(sample(file));
        }
        events
    }

    fn aggregate(events: &[WebhookEvent]) -> WebhookAggregator {
        let mut agg = WebhookAggregator::new();
        for e in events {
            agg.ingest(e);
        }
        agg
    }

    /// The load-bearing parity assertion: the full sample set reconstructs to the
    /// same verdict the spike's `verify-fields.mjs` prints — `Pending` from
    /// `run:build`(Success) + `run:lint`(Pending) + `suite:254`(Success) +
    /// `ctx:ci/deploy-preview`(Success).
    #[test]
    fn full_sample_set_reconstructs_pending_badge() {
        let mut agg = aggregate(&all_samples());
        let out = agg.resolve(&[target()]);

        let PrResolution::Pr(badge) = out.get(&target()).expect("badge present") else {
            panic!("expected a Pr badge");
        };
        assert_eq!(badge.checks, PrCheckState::Pending);
        assert_eq!(badge.number, 42);
        assert!(!badge.is_draft);
        assert_eq!(badge.url, "https://github.com/acme/widget/pull/42");
        assert_eq!(badge.head_oid, SHA);
    }

    #[test]
    fn latest_state_per_identity_wins_and_flips_verdict() {
        // Start from the samples (lint in_progress → Pending overall) …
        let mut events = all_samples();
        // … then a later delivery completes `lint` successfully.
        events.push(WebhookEvent {
            event: "check_run".into(),
            received: 1_721_433_999_000,
            payload: json!({
                "check_run": {
                    "id": 100_002, "name": "lint", "head_sha": SHA,
                    "status": "completed", "conclusion": "success",
                    "check_suite": { "head_branch": BRANCH, "head_sha": SHA }
                },
                "repository": { "name": NAME, "owner": { "login": OWNER } }
            }),
        });
        let mut agg = aggregate(&events);
        let out = agg.resolve(&[target()]);
        let PrResolution::Pr(badge) = out.get(&target()).unwrap() else {
            panic!("expected a Pr badge");
        };
        assert_eq!(badge.checks, PrCheckState::Success);
    }

    #[test]
    fn failure_dominates() {
        let mut events = all_samples();
        events.push(WebhookEvent {
            event: "check_run".into(),
            received: 1_721_433_999_000,
            payload: json!({
                "check_run": {
                    "id": 100_003, "name": "test", "head_sha": SHA,
                    "status": "completed", "conclusion": "failure",
                    "check_suite": { "head_branch": BRANCH, "head_sha": SHA }
                },
                "repository": { "name": NAME, "owner": { "login": OWNER } }
            }),
        });
        let mut agg = aggregate(&events);
        let out = agg.resolve(&[target()]);
        let PrResolution::Pr(badge) = out.get(&target()).unwrap() else {
            panic!("expected a Pr badge");
        };
        assert_eq!(badge.checks, PrCheckState::Failure);
    }

    #[test]
    fn ci_verdict_without_pr_event_is_omitted() {
        // Only CI events — no `pull_request` event, so no browser url/draft.
        let mut events = sample("check_run.json");
        events.extend(sample("check_suite.json"));
        events.extend(sample("status.json"));
        let mut agg = aggregate(&events);
        let out = agg.resolve(&[target()]);
        assert!(
            !out.contains_key(&target()),
            "reconcile poll owns metadata-less targets"
        );
    }

    #[test]
    fn unwatched_target_is_absent_and_pruned() {
        let mut agg = aggregate(&all_samples());
        let other = PrTarget {
            owner: "x".into(),
            name: "y".into(),
            branch: "z".into(),
        };
        let out = agg.resolve(&[other]);
        assert!(out.is_empty());
        // Pruned to the (empty-of-acme) watch set.
        assert!(agg.head.is_empty());
        assert!(agg.checks.is_empty());
        assert!(agg.pr_meta.is_empty());
    }

    #[test]
    fn pr_metadata_persists_across_a_ci_only_pull() {
        // Pull 1: the pull_request event establishes metadata.
        let mut agg = aggregate(&sample("pull_request.json"));
        // Pull 2: a CI-only delta (no PR event) still mints a full badge.
        for e in all_samples().iter().filter(|e| e.event != "pull_request") {
            agg.ingest(e);
        }
        let out = agg.resolve(&[target()]);
        let PrResolution::Pr(badge) = out.get(&target()).unwrap() else {
            panic!("expected a Pr badge");
        };
        assert_eq!(badge.number, 42);
        assert_eq!(badge.checks, PrCheckState::Pending);
    }

    #[test]
    fn ping_and_unrelated_events_are_ignored() {
        let mut agg = WebhookAggregator::new();
        agg.ingest(&WebhookEvent {
            event: "ping".into(),
            received: 1,
            payload: json!({ "zen": "…", "repository": { "name": NAME, "owner": { "login": OWNER } } }),
        });
        assert!(agg.resolve(&[target()]).is_empty());
    }

    #[test]
    fn webhook_event_repo_reads_owner_and_name() {
        let events = sample("pull_request.json");
        assert_eq!(
            events[0].repo(),
            Some(("acme".to_string(), "widget".to_string()))
        );
        let no_repo = WebhookEvent {
            event: "ping".into(),
            received: 1,
            payload: json!({}),
        };
        assert_eq!(no_repo.repo(), None);
    }

    #[test]
    fn from_reply_extracts_events_array() {
        let reply = json!({
            "events": [
                { "event": "ping", "received": 1, "payload": {} },
                { "event": "pull_request", "received": 2, "payload":
                    serde_json::from_str::<Value>(
                        &std::fs::read_to_string(format!(
                            "{}/deploy/webhook-buffer/samples/pull_request.json",
                            env!("CARGO_MANIFEST_DIR")
                        )).unwrap()
                    ).unwrap()["payload"].clone()
                }
            ],
            "cursor": "evt:…",
            "count": 2
        });
        let events = WebhookEvent::from_reply(&reply);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event, "pull_request");
    }
}
