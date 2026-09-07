//! Shared transport core for Google REST APIs.
//!
//! Extracted from `crate::drive::client` (issue #1589) so a second Google
//! host — the Sheets API at `sheets.googleapis.com`, which Drive's
//! `/drive/v3/...`-on-`www.googleapis.com` base URL cannot reach — can reuse
//! the OAuth session, the 401 refresh-and-retry, the quota/429 backoff, the
//! request-log hook and the error-envelope parsing without duplicating any
//! of it.
//!
//! This module owns **transport only**. It has no notion of files, ranges or
//! any API's resource model; `DriveClient` and `SheetsClient` are thin typed
//! wrappers that add a default host, an env override and their own façades.
//! Keeping them as *distinct types* is deliberate: `FilesApi::new` takes a
//! `&DriveClient`, so a Sheets-hosted client cannot be handed to it and
//! silently issue `/drive/v3/files` against `sheets.googleapis.com`.
//!
//! Gmail (`crate::gmail::client::GmailClient`) is a near-verbatim twin of the
//! pre-extraction `DriveClient` and is deliberately **not** migrated here:
//! it carries its own `GmailSession`, so sharing would need a `TokenSource`
//! abstraction that nothing in issue #1589 requires.

use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::{Client, Response};
use url::Url;

use crate::drive::auth::{DriveCredentials, DriveSession};
use crate::drive::error::DriveError;
use crate::request_log;
use crate::utils::http::{connect_timeout, read_timeout, retry_if};

/// Authenticated HTTP transport for one Google API host.
pub(crate) struct GoogleApiClient {
    client: Client,
    base_url: String,
    /// Shared so a `SheetsClient` derived from a `DriveClient` reuses the
    /// same OAuth session: a `sheets write` needs Drive (`files.get` plus the
    /// ancestor-chain walk) *and* Sheets calls, and an unshared session would
    /// refresh the same refresh token twice per invocation.
    session: Arc<DriveSession>,
    /// The `service` tag every HTTP record carries
    /// ([`request_log::record_http_result`]). Deliberately the same for
    /// Drive and Sheets — see `crate::drive::client::SERVICE_TAG`.
    service: &'static str,
    /// Human-readable API name for error messages (`"Drive"`/`"Sheets"`).
    /// Distinct from [`Self::service`] precisely because that one is shared.
    api_name: &'static str,
}

impl std::fmt::Debug for GoogleApiClient {
    // Hand-written, not derived: omits `session` entirely rather than relying
    // on every nested `Secret` staying wrapped — the safest possible
    // redaction is "not mentioned at all."
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleApiClient")
            .field("base_url", &self.base_url)
            .field("service", &self.service)
            .field("api_name", &self.api_name)
            .finish_non_exhaustive()
    }
}

impl GoogleApiClient {
    /// Builds a transport against `base_url` with already-loaded credentials.
    pub(crate) fn new(
        base_url: &str,
        credentials: &DriveCredentials,
        service: &'static str,
        api_name: &'static str,
    ) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(connect_timeout())
            .read_timeout(read_timeout())
            .build()
            .context("Failed to build HTTP client")?;
        let session = Arc::new(DriveSession::new(client.clone(), credentials));
        Ok(Self::from_parts(
            client, base_url, session, service, api_name,
        ))
    }

    /// Builds a transport reusing an existing client and OAuth session,
    /// against a different host.
    ///
    /// This is how a `SheetsClient` is derived from a `DriveClient`: the
    /// `reqwest::Client` (and its connection pool) and the `Arc<DriveSession>`
    /// are shared; only the base URL differs.
    ///
    /// `dead_code`-allowed because its only consumer, `SheetsClient`, lands in
    /// the following commit; the extraction is what makes that possible, so
    /// the seam ships with the extraction rather than trailing it.
    #[allow(dead_code)]
    pub(crate) fn derived(
        &self,
        base_url: &str,
        service: &'static str,
        api_name: &'static str,
    ) -> Self {
        Self::from_parts(
            self.client.clone(),
            base_url,
            Arc::clone(&self.session),
            service,
            api_name,
        )
    }

    fn from_parts(
        client: Client,
        base_url: &str,
        session: Arc<DriveSession>,
        service: &'static str,
        api_name: &'static str,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            session,
            service,
            api_name,
        }
    }

    /// Returns the API base URL (without trailing slash).
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Builds an absolute API URL by joining `path` onto `base_url`.
    ///
    /// Takes `base_url` (rather than `&self`) so the free `build_*_url`
    /// functions in the API façade modules — and their unit tests, which pass
    /// literal base URLs — can call it without an instance.
    ///
    /// `path` is interpolated verbatim, so a caller embedding anything but a
    /// Drive-style opaque id (`[A-Za-z0-9_-]`) **must** percent-encode it
    /// first: a Sheets A1 range can contain `#`, `?`, `/` and spaces, any of
    /// which would silently reshape the URL.
    pub(crate) fn api_url(base_url: &str, path: &str) -> Result<Url> {
        Url::parse(&format!("{base_url}{path}")).context("Invalid Drive base URL")
    }

    /// Appends `segments` to `url`'s path, percent-encoding each one.
    ///
    /// **This is the safe counterpart to [`Self::api_url`]**, and lives
    /// beside it because that method's doc comment describes exactly the
    /// hazard this one exists to remove. A Sheets A1 range goes in the URL
    /// *path* and is caller-influenced text that routinely contains
    /// characters with URL meaning: a sheet titled with a `#` truncates the
    /// path into a fragment, a `?` starts a query string, a `/` invents a
    /// path segment, and a space is simply invalid. Every one of those
    /// silently reads or writes the wrong cells rather than erroring.
    ///
    /// `files_api.rs`'s `format!("/drive/v3/files/{{file_id}}")` sites are
    /// safe only because Drive ids are `[A-Za-z0-9_-]`; do not generalise
    /// from them. New façades should reach for this even when their own ids
    /// happen to be opaque, so no second precedent for `format!` exists.
    pub(crate) fn push_path_segments(url: &mut Url, segments: &[&str]) -> Result<()> {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| anyhow::anyhow!("Invalid API base URL: cannot be a base"))?;
        for segment in segments {
            path.push(segment);
        }
        Ok(())
    }

    /// Checks `response` for success and deserialises its JSON body into `T`.
    pub(crate) async fn parse_response<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        context: &'static str,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(Self::response_to_error(self.api_name, response)
                .await
                .into());
        }
        response.json().await.context(context)
    }

    /// Sends an authenticated GET and deserialises the JSON body into `T`.
    pub(crate) async fn get_parsed<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        context: &'static str,
    ) -> Result<T> {
        let response = self.get_json(url).await?;
        self.parse_response(response, context).await
    }

    /// Sends an authenticated GET request and returns the raw response.
    ///
    /// Retries exactly once on HTTP 401 by forcing a session refresh — see
    /// [`Self::send_authorized`] for why both a proactive and a reactive
    /// refresh path exist.
    pub(crate) async fn get_json(&self, url: &str) -> Result<Response> {
        self.send_authorized(url, "GET", |client, token| {
            client
                .get(url)
                .bearer_auth(token)
                .header("Accept", "application/json")
        })
        .await
    }

    /// Sends an authenticated GET request without forcing an `Accept:
    /// application/json` header, for endpoints that return raw bytes rather
    /// than JSON (`files.export`, `files.get?alt=media`).
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Response> {
        self.send_authorized(url, "GET", |client, token| {
            client.get(url).bearer_auth(token)
        })
        .await
    }

    /// Sends an authenticated POST request with a JSON body.
    pub(crate) async fn post_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<Response> {
        self.send_authorized(url, "POST", |client, token| {
            client
                .post(url)
                .bearer_auth(token)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Sends an authenticated PUT request with a JSON body.
    ///
    /// Added for Sheets' `spreadsheets.values.update` (issue #1589), the
    /// first PUT endpoint this transport calls — Drive v3 uses PATCH for
    /// `files.update`. `dead_code`-allowed until that caller lands; exercised
    /// by `client.rs`'s `put_json_sends_body_and_bearer_auth` meanwhile.
    #[allow(dead_code)]
    pub(crate) async fn put_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<Response> {
        self.send_authorized(url, "PUT", |client, token| {
            client
                .put(url)
                .bearer_auth(token)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Sends an authenticated PATCH request with a JSON body.
    pub(crate) async fn patch_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<Response> {
        self.send_authorized(url, "PATCH", |client, token| {
            client
                .patch(url)
                .bearer_auth(token)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Sends an authenticated POST request with a raw byte body — for
    /// Drive's multipart-upload endpoint, whose `multipart/related` body
    /// [`crate::drive::files_api::FilesApi::upload`] hand-assembles.
    ///
    /// `body` is cloned per send attempt ([`Self::send_authorized`]'s `build`
    /// closure is `Fn`, not `FnOnce` — it may run twice, once on a 401 retry
    /// — and [`reqwest::RequestBuilder::body`] takes ownership).
    pub(crate) async fn post_bytes(
        &self,
        url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Result<Response> {
        self.send_authorized(url, "POST", |client, token| {
            client
                .post(url)
                .bearer_auth(token)
                .header("Content-Type", content_type)
                .body(body.to_vec())
        })
        .await
    }

    /// Sends an authenticated PATCH request with a raw byte body —
    /// [`crate::drive::files_api::FilesApi::edit_content`]'s simple
    /// media-only content replacement.
    pub(crate) async fn patch_bytes(
        &self,
        url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Result<Response> {
        self.send_authorized(url, "PATCH", |client, token| {
            client
                .patch(url)
                .bearer_auth(token)
                .header("Content-Type", content_type)
                .body(body.to_vec())
        })
        .await
    }

    /// Sends a request built by `build`, retrying exactly once on HTTP 401.
    ///
    /// [`DriveSession::access_token`] already refreshes proactively when the
    /// tracked expiry is near — this reactive path exists for what proactive
    /// tracking can't see: clock skew against Google's clock, or the token
    /// being invalidated server-side mid-run (revoked access). A second 401
    /// after the retry is authoritative: either the refresh produced a token
    /// that was also rejected, or another caller's already-current token was
    /// reused and still rejected — either way the problem isn't staleness, so
    /// it surfaces as an ordinary `ApiRequestFailed` rather than retrying
    /// again.
    async fn send_authorized<F>(
        &self,
        url: &str,
        method: &'static str,
        build: F,
    ) -> Result<Response>
    where
        F: Fn(&Client, &str) -> reqwest::RequestBuilder + Send + Sync,
    {
        let token = self
            .session
            .access_token()
            .await
            .context("Failed to obtain a Drive access token")?;
        let response = self
            .send_once(url, method, &build, token.expose_secret())
            .await?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        let refreshed = self
            .session
            .force_refresh(&token)
            .await
            .context("Failed to refresh the Drive access token after a 401")?;
        self.send_once(url, method, &build, refreshed.expose_secret())
            .await
    }

    async fn send_once<F>(
        &self,
        url: &str,
        method: &'static str,
        build: &F,
        token: &str,
    ) -> Result<Response>
    where
        F: Fn(&Client, &str) -> reqwest::RequestBuilder + Send + Sync,
    {
        retry_if(
            || build(&self.client, token),
            |started, result| {
                request_log::record_http_result(self.service, method, url, started, result);
            },
            |status, body| status == 429 || is_quota_exceeded(status, body),
        )
        .await
        .with_context(|| format!("Failed to send {method} request to Drive API"))
    }

    /// Consumes a non-success response into a [`DriveError`].
    ///
    /// Parses Google's error envelope into a human message when present
    /// (falls back to the raw body otherwise). See [`error_reason`] for why
    /// two envelope shapes have to be understood.
    pub(crate) async fn response_to_error(api: &'static str, response: Response) -> DriveError {
        let status = response.status().as_u16();
        let raw = response.text().await.unwrap_or_default();
        let value = serde_json::from_str::<serde_json::Value>(&raw).ok();
        let reason = value.as_ref().and_then(error_reason);
        let body = value
            .as_ref()
            .and_then(error_message)
            .map(|message| match &reason {
                Some(r) => format!("{message} (reason: {r})"),
                None => message,
            })
            .unwrap_or(raw);
        DriveError::ApiRequestFailed {
            api,
            status,
            body,
            reason,
        }
    }
}

/// Extracts a machine-readable reason code from an already-parsed Google
/// JSON error envelope, understanding **both** shapes Google ships.
///
/// Drive v3 and the other older APIs return the legacy envelope, whose code
/// lives at `error.errors[0].reason` (`"insufficientPermissions"`,
/// `"userRateLimitExceeded"`, ...). Newer services — Sheets v4 among them —
/// return the `google.rpc` envelope instead:
///
/// ```json
/// {"error":{"code":403,"message":"...","status":"PERMISSION_DENIED"}}
/// ```
///
/// which has **no `errors[]` array at all**. Reading only the legacy shape
/// yields `reason: None` for every Sheets failure, which silently disables
/// both [`crate::drive::files_api::append_write_scope_hint`] (it matches on
/// the reason string) and [`is_quota_exceeded`] — neither fails loudly, they
/// just never fire. Hence: legacy first, then `error.status`.
fn error_reason(value: &serde_json::Value) -> Option<String> {
    let error = value.get("error")?;
    let legacy = error
        .get("errors")
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("reason"))
        .and_then(|r| r.as_str())
        .map(str::to_string);
    legacy.or_else(|| error.get("status")?.as_str().map(str::to_string))
}

/// Extracts the `error.message` field from an already-parsed Google JSON
/// error envelope, if present. Both envelope shapes carry it identically.
fn error_message(value: &serde_json::Value) -> Option<String> {
    value
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

/// Whether a response is Google's quota-exhaustion signal — a **403**
/// carrying the legacy `userRateLimitExceeded` reason or the `google.rpc`
/// `RESOURCE_EXHAUSTED` status, not any 403 with a reason field:
/// `insufficientPermissions` / `PERMISSION_DENIED` are also 403s and must
/// never be retried (retrying a scope error just wastes the backoff window
/// before failing anyway).
///
/// Unlike Gmail's `is_gmail_quota_exceeded`, this does **not** also match the
/// bare `rateLimitExceeded` reason — that string is confirmed for Gmail but
/// not for Drive against
/// [Drive's error-handling guide](https://developers.google.com/workspace/drive/api/guides/handle-errors);
/// widen this match only once testing surfaces it. Plain `429` responses are
/// already covered by the literal `status == 429` branch in
/// [`GoogleApiClient::send_once`], independent of this function.
fn is_quota_exceeded(status: u16, body: &[u8]) -> bool {
    if status != 403 {
        return false;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    matches!(
        error_reason(&value).as_deref(),
        Some("userRateLimitExceeded" | "RESOURCE_EXHAUSTED")
    )
}

/// Test-only seam letting sibling modules bootstrap a deterministic access
/// token against a wiremock `/token` endpoint.
///
/// A **constructor**, not a post-hoc field assignment: with the session held
/// behind an `Arc` and shared into derived clients, replacing it after a
/// derivation would leave the derived client pointed at the real
/// `oauth2.googleapis.com` — a live network call from a unit test.
#[cfg(test)]
impl GoogleApiClient {
    pub(crate) fn new_with_token_endpoint(
        base_url: &str,
        credentials: &DriveCredentials,
        service: &'static str,
        api_name: &'static str,
        token_endpoint: &str,
    ) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(connect_timeout())
            .read_timeout(read_timeout())
            .build()
            .context("Failed to build HTTP client")?;
        let session = Arc::new(DriveSession::new_with_token_endpoint(
            client.clone(),
            credentials,
            token_endpoint,
        ));
        Ok(Self::from_parts(
            client, base_url, session, service, api_name,
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::DriveGrantedScopes;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    // ── error_reason: both Google envelope shapes ──────────────────────

    #[test]
    fn error_reason_reads_the_legacy_drive_envelope() {
        let value = serde_json::json!({
            "error": {"errors": [{"reason": "insufficientPermissions"}], "message": "nope"},
        });
        assert_eq!(
            error_reason(&value).as_deref(),
            Some("insufficientPermissions")
        );
    }

    #[test]
    fn error_reason_reads_the_google_rpc_status_when_there_is_no_errors_array() {
        // The shape Sheets v4 actually returns. Before issue #1589 this
        // yielded None, silently disabling the write-scope hint and the
        // quota retry for every Sheets call.
        let value = serde_json::json!({
            "error": {"code": 403, "message": "The caller does not have permission",
                      "status": "PERMISSION_DENIED"},
        });
        assert_eq!(error_reason(&value).as_deref(), Some("PERMISSION_DENIED"));
    }

    #[test]
    fn error_reason_prefers_the_legacy_reason_when_both_are_present() {
        let value = serde_json::json!({
            "error": {"errors": [{"reason": "insufficientPermissions"}],
                      "status": "PERMISSION_DENIED"},
        });
        assert_eq!(
            error_reason(&value).as_deref(),
            Some("insufficientPermissions")
        );
    }

    #[test]
    fn error_reason_none_when_neither_shape_is_present() {
        assert_eq!(error_reason(&serde_json::json!({"error": {}})), None);
        assert_eq!(error_reason(&serde_json::json!({"nope": 1})), None);
    }

    // ── is_quota_exceeded ──────────────────────────────────────────────

    #[test]
    fn is_quota_exceeded_false_on_non_utf8_body() {
        assert!(!is_quota_exceeded(403, &[0xff, 0xfe]));
    }

    #[test]
    fn is_quota_exceeded_false_on_non_403_status() {
        assert!(!is_quota_exceeded(
            429,
            br#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#
        ));
    }

    #[test]
    fn is_quota_exceeded_true_on_matching_403_reason() {
        assert!(is_quota_exceeded(
            403,
            br#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#
        ));
    }

    #[test]
    fn is_quota_exceeded_true_on_google_rpc_resource_exhausted() {
        assert!(is_quota_exceeded(
            403,
            br#"{"error":{"code":403,"status":"RESOURCE_EXHAUSTED"}}"#
        ));
    }

    #[test]
    fn is_quota_exceeded_false_on_permission_denied() {
        // The whole point of not matching every 403 with a reason: a scope
        // error must fail immediately, not burn the backoff window first.
        assert!(!is_quota_exceeded(
            403,
            br#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#
        ));
        assert!(!is_quota_exceeded(
            403,
            br#"{"error":{"errors":[{"reason":"insufficientPermissions"}]}}"#
        ));
    }

    // ── construction ───────────────────────────────────────────────────

    #[test]
    fn new_strips_trailing_slash() {
        let client = GoogleApiClient::new(
            "https://example.test/",
            &test_credentials(),
            "drive",
            "Drive",
        )
        .unwrap();
        assert_eq!(client.base_url(), "https://example.test");
    }

    #[test]
    fn derived_shares_the_session_and_swaps_the_host() {
        let drive = GoogleApiClient::new(
            "https://www.googleapis.com",
            &test_credentials(),
            "drive",
            "Drive",
        )
        .unwrap();
        let sheets = drive.derived("https://sheets.googleapis.com", "drive", "Sheets");
        assert_eq!(sheets.base_url(), "https://sheets.googleapis.com");
        assert_eq!(drive.base_url(), "https://www.googleapis.com");
        assert!(
            Arc::ptr_eq(&drive.session, &sheets.session),
            "a derived client must reuse the OAuth session, not open a second one"
        );
    }

    #[test]
    fn debug_never_mentions_the_session_field() {
        let client = GoogleApiClient::new(
            "https://example.test",
            &test_credentials(),
            "drive",
            "Drive",
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret-1"));
        assert!(!debug.contains("refresh-1"));
        assert!(!debug.contains("session"));
    }
}
