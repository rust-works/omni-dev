//! Config and outbound pull client for the #1378 webhook buffer.
//!
//! The daemon's `webhook` PR source (in [`crate::daemon::services::worktrees`])
//! pulls buffered GitHub deliveries from an operator-deployed Cloudflare Worker +
//! KV buffer (`deploy/webhook-buffer/`) over an **outbound** HTTPS connection —
//! nothing local is exposed, so ADR-0036's loopback trust model is preserved. The
//! pull is authenticated with a read token; the webhook **HMAC secret never
//! reaches the daemon** (it lives only in the Worker). See ADR-0055.
//!
//! This module owns two small things: [`WebhookBufferConfig`] (the buffer URL and
//! read token, resolved from env → settings via [`Settings::get_env_var`], the
//! [`crate::snowflake`] `from_env_and_settings` precedent) and
//! [`WebhookBufferClient`] (a thin reqwest wrapper over the Worker's
//! `GET /events?since=<cursor>` endpoint). Cadence lives with the other daemon
//! poll knobs in the worktrees service, not here.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::pr_status::webhook::WebhookEvent;
use crate::utils::settings::Settings;

/// Base URL of the deployed webhook-buffer Worker (e.g.
/// `https://omni-dev-webhook-buffer.<you>.workers.dev`).
const ENV_BUFFER_URL: &str = "OMNI_DEV_WEBHOOK_BUFFER_URL";
/// The read/pull token the daemon presents on `GET /events` (Bearer auth).
const ENV_READ_TOKEN: &str = "OMNI_DEV_WEBHOOK_READ_TOKEN";
/// A file to read the read token from, wins over [`ENV_READ_TOKEN`] when set —
/// the `SNOWFLAKE_PRIVATE_KEY_PATH` precedent, so the secret need not sit inline
/// in `settings.json` or the environment.
const ENV_READ_TOKEN_PATH: &str = "OMNI_DEV_WEBHOOK_READ_TOKEN_PATH";

/// The HTTP request timeout for one buffer pull. Generous — the pull is a small
/// KV list + a handful of value reads, but a wedged Worker must not stall the
/// source loop forever.
const PULL_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolved connection details for the webhook buffer.
///
/// Both fields are `Option` so a partially-configured daemon (e.g. URL set, token
/// missing) degrades to the reconcile poll rather than erroring —
/// [`is_configured`](Self::is_configured) gates whether the fast pull runs at all.
#[derive(Debug, Clone, Default)]
pub struct WebhookBufferConfig {
    /// The Worker base URL, or `None` when unset.
    pub base_url: Option<String>,
    /// The read/pull token, or `None` when unset.
    pub read_token: Option<String>,
}

impl WebhookBufferConfig {
    /// Resolves the buffer config from process env → active profile → base env,
    /// entirely via [`Settings::get_env_var`] (which owns the precedence). The read
    /// token may be given inline ([`ENV_READ_TOKEN`]) or as a file path
    /// ([`ENV_READ_TOKEN_PATH`], which wins) — the Snowflake private-key-path
    /// pattern. Blank values are treated as unset.
    pub fn from_env_and_settings() -> Result<Self> {
        let settings = Settings::load().unwrap_or_default();
        let base_url = settings.get_env_var(ENV_BUFFER_URL).and_then(non_blank);
        let read_token = match settings
            .get_env_var(ENV_READ_TOKEN_PATH)
            .and_then(non_blank)
        {
            Some(path) => {
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {ENV_READ_TOKEN_PATH} '{path}'"))?;
                non_blank(raw)
            }
            None => settings.get_env_var(ENV_READ_TOKEN).and_then(non_blank),
        };
        Ok(Self {
            base_url,
            read_token,
        })
    }

    /// Whether both the URL and token are present — the precondition for running
    /// the fast buffer pull. When false the `webhook` source still runs its
    /// reconcile poll, so badges keep working (just without the real-time push).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.base_url.is_some() && self.read_token.is_some()
    }
}

/// One page of buffered deliveries plus the cursor to resume from.
pub(crate) struct EventsPage {
    /// The deliveries in this page (already parsed; malformed ones dropped).
    pub events: Vec<WebhookEvent>,
    /// The cursor to pass as `since` on the next pull. Unchanged from the request
    /// cursor when the page was empty.
    pub cursor: String,
    /// Whether the buffer has more events beyond this page (drain by pulling again
    /// with the returned cursor).
    pub more: bool,
}

/// A thin authenticated client over the buffer Worker's `GET /events` endpoint.
pub(crate) struct WebhookBufferClient {
    events_url: String,
    token: String,
    http: reqwest::Client,
}

impl WebhookBufferClient {
    /// Builds a client for `base_url` presenting `token` as a Bearer credential.
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(PULL_TIMEOUT)
            .build()
            .context("building webhook-buffer HTTP client")?;
        Ok(Self {
            events_url: format!("{}/events", base_url.trim_end_matches('/')),
            token: token.to_string(),
            http,
        })
    }

    /// Pulls one page of events strictly after `since`. The token rides the
    /// `Authorization: Bearer` header (never logged, never a query param), keeping
    /// it off any request log — ADR-0043/0055.
    pub async fn fetch_events(&self, since: &str) -> Result<EventsPage> {
        let mut url =
            reqwest::Url::parse(&self.events_url).context("invalid webhook buffer URL")?;
        url.query_pairs_mut().append_pair("since", since);
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("pulling webhook buffer events")?;

        let status = resp.status();
        if !status.is_success() {
            // Body only — never the token — in the error, so nothing secret leaks.
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "webhook buffer /events returned {status}: {}",
                snippet(&body)
            );
        }

        let body: Value = resp.json().await.context("decoding /events body")?;
        let events = WebhookEvent::from_reply(&body);
        let cursor = body
            .get("cursor")
            .and_then(Value::as_str)
            .unwrap_or(since)
            .to_string();
        let more = body.get("more").and_then(Value::as_bool).unwrap_or(false);
        Ok(EventsPage {
            events,
            cursor,
            more,
        })
    }
}

/// Trims whitespace and treats a now-empty string as unset — so a blank env var or
/// a token file that is only a trailing newline resolves to `None`.
fn non_blank(s: String) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A bounded excerpt of a response body for an error message. Truncates on a
/// **character** boundary so an arbitrary UTF-8 body can never panic a slice.
fn snippet(body: &str) -> String {
    const MAX: usize = 200;
    if body.chars().count() <= MAX {
        body.to_string()
    } else {
        let truncated: String = body.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn non_blank_treats_whitespace_as_unset() {
        assert_eq!(non_blank("  ".into()), None);
        assert_eq!(non_blank("\n".into()), None);
        assert_eq!(non_blank(" tok \n".into()), Some("tok".to_string()));
    }

    #[test]
    fn is_configured_requires_both_url_and_token() {
        assert!(!WebhookBufferConfig::default().is_configured());
        assert!(!WebhookBufferConfig {
            base_url: Some("https://x".into()),
            read_token: None,
        }
        .is_configured());
        assert!(WebhookBufferConfig {
            base_url: Some("https://x".into()),
            read_token: Some("t".into()),
        }
        .is_configured());
    }

    #[test]
    fn client_appends_events_path_and_trims_trailing_slash() {
        let c = WebhookBufferClient::new("https://buf.example.dev/", "tok").unwrap();
        assert_eq!(c.events_url, "https://buf.example.dev/events");
    }

    #[test]
    fn snippet_bounds_long_bodies() {
        assert_eq!(snippet("short"), "short");
        // 200 body chars + the single-char `…` ellipsis (which is 3 UTF-8 bytes).
        let bounded = snippet(&"x".repeat(500));
        assert_eq!(bounded.chars().count(), 201);
        assert!(bounded.ends_with('…'));
    }
}
