//! GitHub API rate-limit resolution for the daemon's budget monitor (#1375).
//!
//! The engine half of the rate-limit view, mirroring the [`crate::pr_status`]
//! engine / [`crate::daemon::services::worktrees`] adapter split: this module is
//! pure resolution — run `gh api rate_limit`, parse the reply, compute per-resource
//! used-percentage — and knows nothing about the daemon, the socket, or the tray.
//! The adapter owns the poll loop and surfaces the cached snapshot.
//!
//! # Why this exists
//!
//! The daemon's worktrees PR-badge poller ([`crate::pr_status`]) shells out to `gh`,
//! spending the same GitHub API budget as every other tool sharing the user's `gh`
//! token. When that consumption spikes the budget silently drains to zero and
//! rate-limits `gh` machine-wide, with no visibility until commands start failing.
//! This module lets the daemon watch the *trend* and warn before exhaustion.
//!
//! # Why polling it is free
//!
//! GitHub documents `GET /rate_limit` as **not counting against your rate limit**
//! (confirmed empirically: consecutive `gh api rate_limit` calls leave `core.used`
//! flat). So this monitor adds **zero** cost to the very budget it watches — the
//! poller can run on a fixed cadence without a budget concern of its own.
//!
//! # No credential enters the daemon
//!
//! Resolution shells out to `gh api rate_limit`, so the GitHub token stays inside
//! `gh` where the user already put it — exactly as the PR poller does (ADR-0050),
//! following ADR-0003's "shell out to `gh`/`git` for GitHub operations".

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The used-percentage at or above which a resource is treated as "high".
///
/// A resource over this is highlighted in `daemon status`, logged as a warning by
/// the poller, and marked in the tray. ~80% is the point at which an approaching
/// exhaustion is worth catching before it bites.
pub const WARN_PERCENT: f64 = 80.0;

/// The `/rate_limit` resources this monitor surfaces, in display order. `core`
/// (REST) and `graphql` are the two the daemon's own `gh` usage spends; `search`
/// is included because it is a common third consumer and costs nothing extra to
/// report.
const RESOURCE_KEYS: &[&str] = &["graphql", "core", "search"];

/// One resource's budget usage, parsed from a `/rate_limit` `resources.<name>`
/// object.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateLimitResource {
    /// Requests spent in the current window.
    pub used: u64,
    /// The window's ceiling.
    pub limit: u64,
    /// Requests remaining (`limit - used`, as GitHub reports it).
    pub remaining: u64,
    /// `used / limit * 100`, rounded to one decimal. `0.0` when `limit` is `0`, so
    /// a missing or zero ceiling never divides by zero or reads as "over budget".
    pub percent: f64,
    /// Unix epoch (seconds) at which the window resets. Rendered as `HH:MMZ` by
    /// [`format_reset_utc`]; kept raw on the wire so machines can compute their own.
    pub reset: i64,
}

impl RateLimitResource {
    /// Reads one `resources.<name>` object, computing [`percent`](Self::percent).
    /// `None` when the object is absent or missing `used`/`limit` — a partial reply
    /// surfaces the resources it *does* carry rather than failing the whole poll.
    fn from_value(v: &Value) -> Option<Self> {
        let used = v.get("used").and_then(Value::as_u64)?;
        let limit = v.get("limit").and_then(Value::as_u64)?;
        // `remaining` is usually present; derive it defensively when it is not.
        let remaining = v
            .get("remaining")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| limit.saturating_sub(used));
        let reset = v.get("reset").and_then(Value::as_i64).unwrap_or(0);
        Some(Self {
            used,
            limit,
            remaining,
            percent: percent_of(used, limit),
            reset,
        })
    }

    /// Whether this resource is at or above the [`WARN_PERCENT`] threshold.
    #[must_use]
    pub fn over_warn(&self) -> bool {
        self.percent >= WARN_PERCENT
    }
}

/// `used / limit * 100`, rounded to one decimal, guarding `limit == 0` → `0.0`.
fn percent_of(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        return 0.0;
    }
    // Round to one decimal so the value is stable to serialize and compare (raw
    // f64 division prints long tails that make the wire and tests noisy).
    let raw = used as f64 / limit as f64 * 100.0;
    (raw * 10.0).round() / 10.0
}

/// A parsed `gh api rate_limit` snapshot: the resources this monitor surfaces.
///
/// Every field is optional and skipped when absent, so the JSON on the daemon's
/// status payload carries only what `gh` actually returned (the daemon's
/// forward-compat contract). In practice `graphql` and `core` are always present.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    /// The GraphQL API budget — what the PR-badge poller spends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql: Option<RateLimitResource>,
    /// The REST (core) API budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<RateLimitResource>,
    /// The search API budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<RateLimitResource>,
}

impl RateLimitSnapshot {
    /// The resources in display order, each paired with its name, skipping any that
    /// were absent from the reply.
    fn resources(&self) -> Vec<(&'static str, RateLimitResource)> {
        [
            ("graphql", self.graphql),
            ("core", self.core),
            ("search", self.search),
        ]
        .into_iter()
        .filter_map(|(name, res)| res.map(|r| (name, r)))
        .collect()
    }

    /// The highest used-percentage across present resources, or `0.0` when empty.
    #[must_use]
    pub fn max_percent(&self) -> f64 {
        self.resources()
            .iter()
            .map(|(_, r)| r.percent)
            .fold(0.0_f64, f64::max)
    }

    /// Whether any present resource is at or above [`WARN_PERCENT`].
    #[must_use]
    pub fn over_warn(&self) -> bool {
        self.resources().iter().any(|(_, r)| r.over_warn())
    }

    /// The one-line summary for `daemon status`, e.g.
    /// `graphql 82% (4100/5000, resets 06:50Z) · core 3% (27/1000)`, prefixed with
    /// `⚠ ` when any resource is over the warn threshold. Empty when no resource is
    /// present (the caller then prints nothing).
    #[must_use]
    pub fn summary_line(&self) -> String {
        let body = self
            .resources()
            .iter()
            .map(|(name, r)| format_resource(name, r))
            .collect::<Vec<_>>()
            .join(" · ");
        if body.is_empty() {
            return body;
        }
        if self.over_warn() {
            format!("⚠ {body}")
        } else {
            body
        }
    }

    /// The compact tray label, e.g. `github: graphql 82% · core 3%`, with a
    /// trailing `⚠` when over the warn threshold. Empty when no resource is present.
    #[must_use]
    pub fn tray_label(&self) -> String {
        let resources = self.resources();
        if resources.is_empty() {
            return String::new();
        }
        let body = resources
            .iter()
            .map(|(name, r)| format!("{name} {}%", trim_percent(r.percent)))
            .collect::<Vec<_>>()
            .join(" · ");
        if self.over_warn() {
            format!("github: {body} ⚠")
        } else {
            format!("github: {body}")
        }
    }
}

/// Formats one resource as `graphql 82% (4100/5000, resets 06:50Z)`.
fn format_resource(name: &str, r: &RateLimitResource) -> String {
    format!(
        "{name} {}% ({}/{}, resets {})",
        trim_percent(r.percent),
        r.used,
        r.limit,
        format_reset_utc(r.reset)
    )
}

/// Renders a percentage without a trailing `.0` (so `82.0` prints `82`, `81.5`
/// prints `81.5`) — the usual budget reading is whole-number and the decimal is
/// noise.
fn trim_percent(percent: f64) -> String {
    if (percent.fract()).abs() < f64::EPSILON {
        format!("{}", percent as i64)
    } else {
        format!("{percent}")
    }
}

/// Formats a unix epoch as a `HH:MMZ` UTC clock time, e.g. `06:50Z`. A zero or
/// unparseable epoch renders `??:??Z` rather than a misleading `00:00Z`.
#[must_use]
pub fn format_reset_utc(epoch: i64) -> String {
    match DateTime::<Utc>::from_timestamp(epoch, 0) {
        Some(dt) if epoch > 0 => dt.format("%H:%MZ").to_string(),
        _ => "??:??Z".to_string(),
    }
}

/// Parses a `gh api rate_limit` reply into a [`RateLimitSnapshot`]. Best-effort per
/// resource: a resource absent or malformed is simply omitted rather than sinking
/// the whole snapshot.
fn parse_rate_limit(body: &Value) -> RateLimitSnapshot {
    let resources = body.get("resources");
    let read = |key: &str| -> Option<RateLimitResource> {
        resources
            .and_then(|r| r.get(key))
            .and_then(RateLimitResource::from_value)
    };
    debug_assert!(RESOURCE_KEYS.contains(&"graphql"));
    RateLimitSnapshot {
        graphql: read("graphql"),
        core: read("core"),
        search: read("search"),
    }
}

/// Runs one `gh api rate_limit` call against `bin`. **Blocking** — callers must be
/// on a blocking thread, never an async worker. Mirrors
/// [`crate::pr_status`]'s `run_gh_graphql`.
fn run_gh_rate_limit(bin: &Path) -> Result<Value> {
    let output = crate::github_metrics::run_gh(bin, ["api", "rate_limit"], "api rate_limit", None)
        .with_context(|| {
            format!(
                "failed to run {} (is the GitHub CLI installed?)",
                bin.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api rate_limit failed: {}", stderr.trim());
    }
    serde_json::from_slice(&output.stdout).context("gh api rate_limit returned invalid JSON")
}

/// Resolves the current rate-limit snapshot in **one** `gh api rate_limit` call,
/// using the `gh` at `bin`.
///
/// The binary is a parameter rather than resolved here so callers read the
/// environment **once** (the poller does it at spawn) and so tests inject a stub
/// without mutating the process environment — the same discipline as
/// [`crate::pr_status::resolve_with`]. Reuse [`crate::pr_status::resolve_gh_binary`]
/// to obtain `bin`.
///
/// **Blocking** — run on a blocking thread. A missing, unauthenticated, or failing
/// `gh` yields `Err`, which the poller logs and shrugs off (keeping the last good
/// snapshot); querying `/rate_limit` spends nothing, so this call never affects the
/// budget it reports.
pub fn resolve_rate_limit_with(bin: &Path) -> Result<RateLimitSnapshot> {
    let body = run_gh_rate_limit(bin)?;
    Ok(parse_rate_limit(&body))
}

/// The poller-written, status/menu-read rate-limit snapshot cache.
///
/// A plain `std::Mutex<Option<..>>`: writes come from the poll loop, reads from the
/// status op and the tray menu build. The lock is never held across an `.await` —
/// every method takes it, finishes, and drops it. Empty until the first poll lands,
/// in which case `daemon status` simply carries no `github_rate_limit` field.
#[derive(Debug, Default)]
pub struct RateLimitCache {
    snapshot: Mutex<Option<RateLimitSnapshot>>,
}

impl RateLimitCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest snapshot, or `None` before the first successful poll.
    #[must_use]
    pub fn get(&self) -> Option<RateLimitSnapshot> {
        *self.lock()
    }

    /// Stores `next`, returning whether it differs from the previous snapshot — the
    /// caller uses the bool only to decide whether to log a transition, never to
    /// bump the tree change-notify (rate limit is not tree topology).
    pub fn replace(&self, next: RateLimitSnapshot) -> bool {
        let mut guard = self.lock();
        let changed = *guard != Some(next);
        *guard = Some(next);
        changed
    }

    /// Poison-tolerant lock: a panicking holder must not wedge the monitor, which is
    /// best-effort decoration.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<RateLimitSnapshot>> {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
// `float_cmp`: the percentages are rounded to one decimal by `percent_of`, so the
// expected literals are bit-identical to the computed values — an exact `==` is
// correct here, not the usual float-equality hazard.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::test_support::shim::{retry_on_etxtbsy, shim_lock, write_exec_script};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::MutexGuard;

    /// A realistic `gh api rate_limit` reply body.
    fn sample_body() -> Value {
        json!({
            "resources": {
                "core": {"limit": 5000, "used": 27, "remaining": 4973, "reset": 1_700_000_000_i64},
                "graphql": {"limit": 5000, "used": 4100, "remaining": 900, "reset": 1_700_003_000_i64},
                "search": {"limit": 30, "used": 3, "remaining": 27, "reset": 1_700_000_060_i64},
            },
            // The deprecated top-level `rate` alias of core — ignored.
            "rate": {"limit": 5000, "used": 27, "remaining": 4973, "reset": 1_700_000_000_i64}
        })
    }

    // --- Parsing & percentage math ---

    #[test]
    fn parse_reads_every_resource_and_computes_percent() {
        let snap = parse_rate_limit(&sample_body());
        let graphql = snap.graphql.expect("graphql present");
        assert_eq!(graphql.used, 4100);
        assert_eq!(graphql.limit, 5000);
        assert_eq!(graphql.remaining, 900);
        assert_eq!(graphql.percent, 82.0);
        assert_eq!(graphql.reset, 1_700_003_000);
        let core = snap.core.expect("core present");
        assert_eq!(core.percent, 0.5);
        assert!(snap.search.is_some());
    }

    #[test]
    fn parse_tolerates_a_missing_search_resource() {
        let body = json!({"resources": {
            "core": {"limit": 5000, "used": 27, "remaining": 4973, "reset": 1},
            "graphql": {"limit": 5000, "used": 10, "remaining": 4990, "reset": 1},
        }});
        let snap = parse_rate_limit(&body);
        assert!(snap.graphql.is_some());
        assert!(snap.core.is_some());
        assert!(snap.search.is_none());
    }

    #[test]
    fn parse_tolerates_a_missing_resources_block() {
        let snap = parse_rate_limit(&json!({}));
        assert_eq!(snap, RateLimitSnapshot::default());
    }

    #[test]
    fn parse_derives_remaining_when_absent() {
        let body = json!({"resources": {"core": {"limit": 100, "used": 40}}});
        let core = parse_rate_limit(&body).core.expect("core present");
        assert_eq!(core.remaining, 60);
        assert_eq!(core.percent, 40.0);
    }

    #[test]
    fn percent_guards_a_zero_limit() {
        assert_eq!(percent_of(0, 0), 0.0);
        assert_eq!(percent_of(5, 0), 0.0);
        assert_eq!(percent_of(1, 3), 33.3);
    }

    // --- Warn threshold ---

    #[test]
    fn over_warn_fires_only_at_or_above_the_threshold() {
        let res = |used: u64| RateLimitResource {
            used,
            limit: 100,
            remaining: 100 - used,
            percent: percent_of(used, 100),
            reset: 0,
        };
        assert!(!res(79).over_warn());
        assert!(res(80).over_warn());
        assert!(res(95).over_warn());

        let snap = RateLimitSnapshot {
            core: Some(res(10)),
            graphql: Some(res(85)),
            search: None,
        };
        assert!(snap.over_warn());
        assert_eq!(snap.max_percent(), 85.0);
    }

    // --- Formatting ---

    #[test]
    fn format_reset_utc_renders_clock_time_or_a_placeholder() {
        // 1700000000 = 2023-11-14T22:13:20Z.
        assert_eq!(format_reset_utc(1_700_000_000), "22:13Z");
        assert_eq!(format_reset_utc(0), "??:??Z");
        assert_eq!(format_reset_utc(-5), "??:??Z");
    }

    #[test]
    fn summary_line_lists_resources_and_marks_the_warn_case() {
        let snap = parse_rate_limit(&sample_body());
        let line = snap.summary_line();
        assert!(line.contains("graphql 82% (4100/5000, resets"), "{line}");
        assert!(line.contains("core 0.5% (27/5000"), "{line}");
        // graphql is at 82% ≥ 80, so the whole line is flagged.
        assert!(line.starts_with("⚠ "), "{line}");
    }

    #[test]
    fn summary_line_omits_the_marker_below_threshold() {
        let body = json!({"resources": {
            "graphql": {"limit": 5000, "used": 10, "remaining": 4990, "reset": 1_700_000_000_i64},
            "core": {"limit": 5000, "used": 27, "remaining": 4973, "reset": 1_700_000_000_i64},
        }});
        let line = parse_rate_limit(&body).summary_line();
        assert!(!line.starts_with('⚠'), "{line}");
        assert!(line.starts_with("graphql 0.2%"), "{line}");
    }

    #[test]
    fn summary_line_is_empty_without_resources() {
        assert!(RateLimitSnapshot::default().summary_line().is_empty());
        assert!(RateLimitSnapshot::default().tray_label().is_empty());
    }

    #[test]
    fn tray_label_is_compact_and_marks_the_warn_case() {
        let snap = parse_rate_limit(&sample_body());
        let label = snap.tray_label();
        assert!(label.starts_with("github: graphql 82%"), "{label}");
        assert!(label.ends_with('⚠'), "{label}");
    }

    #[test]
    fn tray_label_omits_the_marker_below_threshold() {
        let body = json!({"resources": {
            "graphql": {"limit": 5000, "used": 10, "remaining": 4990, "reset": 1},
            "core": {"limit": 5000, "used": 27, "remaining": 4973, "reset": 1},
        }});
        let label = parse_rate_limit(&body).tray_label();
        assert_eq!(label, "github: graphql 0.2% · core 0.5%", "{label}");
        assert!(!label.contains('⚠'), "{label}");
    }

    // --- Cache ---

    #[test]
    fn cache_get_and_replace_report_changes() {
        let cache = RateLimitCache::new();
        assert!(cache.get().is_none());
        let a = parse_rate_limit(&sample_body());
        // First write is a change.
        assert!(cache.replace(a));
        assert_eq!(cache.get(), Some(a));
        // An identical write is not.
        assert!(!cache.replace(a));
        // A different snapshot is.
        let b = parse_rate_limit(&json!({"resources": {
            "graphql": {"limit": 5000, "used": 4200, "remaining": 800, "reset": 1}
        }}));
        assert!(cache.replace(b));
    }

    // --- resolve_rate_limit_with: the degradation contract ---
    //
    // A missing, unauthenticated, or failing `gh` must surface an error to the
    // poller (which keeps the last good snapshot) and never panic or hang. Mirrors
    // `pr_status`'s `fake_gh` shim tests.

    /// Writes an executable stub standing in for `gh`, printing `stdout` and exiting
    /// `code`. Returns the shim serialisation lock alongside the path (hold it until
    /// the exec is done); pair the exec with [`retry_on_etxtbsy`].
    fn fake_gh(dir: &Path, stdout: &str, code: i32) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!("#!/bin/sh\ncat <<'JSON'\n{stdout}\nJSON\nexit {code}\n"),
        );
        (path, guard)
    }

    #[test]
    fn resolve_errors_when_gh_is_missing() {
        let err = resolve_rate_limit_with(Path::new("/no/such/gh/xyzzy")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to run"), "{msg}");
        assert!(msg.contains("GitHub CLI"), "{msg}");
    }

    #[test]
    fn resolve_errors_on_a_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "", 1);
        let err = retry_on_etxtbsy(|| resolve_rate_limit_with(&bin)).unwrap_err();
        assert!(
            format!("{err:#}").contains("gh api rate_limit failed"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_errors_on_unparseable_output() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "not json at all", 0);
        let err = retry_on_etxtbsy(|| resolve_rate_limit_with(&bin)).unwrap_err();
        assert!(format!("{err:#}").contains("invalid JSON"), "{err:#}");
    }

    #[test]
    fn resolve_reads_a_real_reply_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), &sample_body().to_string(), 0);
        let snap = retry_on_etxtbsy(|| resolve_rate_limit_with(&bin)).unwrap();
        assert_eq!(snap.graphql.unwrap().percent, 82.0);
        assert_eq!(snap.core.unwrap().used, 27);
    }
}
