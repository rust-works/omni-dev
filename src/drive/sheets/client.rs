//! Sheets v4 REST API client.
//!
//! A typed wrapper around [`GoogleApiClient`] — the same transport
//! `DriveClient` uses — pinned to `sheets.googleapis.com`. Everything
//! interesting (OAuth session, 401 refresh-and-retry, quota backoff,
//! request logging, error-envelope parsing) is inherited, so this module is
//! only a default host, an env override, and a distinct *type*.
//!
//! That distinct type is the point, not incidental: `FilesApi::new` takes a
//! `&DriveClient`, so a Sheets-hosted client cannot be handed to it and
//! silently issue `/drive/v3/files` against the Sheets host. A shared
//! `DriveClient` re-based onto a second URL would compile.

use anyhow::Result;

use crate::drive::api_client::GoogleApiClient;
use crate::drive::auth::DriveCredentials;
use crate::drive::client::{DriveClient, SERVICE_TAG};
use crate::utils::env::{EnvSource, SystemEnv};

/// Environment variable overriding the Sheets API host wholesale.
///
/// Mirrors `DRIVE_API_URL`/`GMAIL_API_URL`; process-env only, never written
/// to `settings.json`. Used to point at a wiremock server or route through a
/// forced egress proxy.
pub const SHEETS_API_URL: &str = "SHEETS_API_URL";

/// HTTP client for the Sheets v4 REST API.
pub struct SheetsClient {
    inner: GoogleApiClient,
}

impl std::fmt::Debug for SheetsClient {
    // Hand-written for the same reason `DriveClient`'s is: never mention the
    // session, rather than trusting every nested `Secret` to stay wrapped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SheetsClient")
            .field("base_url", &self.inner.base_url())
            .finish_non_exhaustive()
    }
}

impl SheetsClient {
    /// The real Sheets API host.
    ///
    /// Unlike Drive — which lives on the general `www.googleapis.com` with a
    /// `/drive/v3/...` prefix — Sheets has its own subdomain, so no base-URL
    /// tweak to `DriveClient` could ever reach it.
    const DEFAULT_BASE_URL: &'static str = "https://sheets.googleapis.com";

    /// Builds a client against `base_url` with already-loaded credentials.
    ///
    /// Prefer [`Self::from_drive_client`] when a `DriveClient` is already in
    /// hand: this constructor opens a *second* OAuth session, which means a
    /// second token refresh per invocation.
    pub fn new(base_url: &str, credentials: &DriveCredentials) -> Result<Self> {
        Ok(Self {
            inner: GoogleApiClient::new(base_url, credentials, SERVICE_TAG)?,
        })
    }

    /// Derives a Sheets client from a Drive client, sharing its OAuth session
    /// and connection pool.
    ///
    /// This is the constructor every real caller wants. A `sheets write` has
    /// to talk to *both* APIs — Drive for `files.get` and the ancestor-chain
    /// walk that feeds the write gate, Sheets for the cells — and two
    /// independent sessions would refresh the same refresh token twice for
    /// one command.
    ///
    /// Honours `SHEETS_API_URL`; the Drive client's own base URL is
    /// irrelevant here, since the two hosts are unrelated.
    pub fn from_drive_client(drive: &DriveClient) -> Result<Self> {
        Self::from_drive_client_with(&SystemEnv, drive)
    }

    /// [`from_drive_client`](Self::from_drive_client) over an injected
    /// [`EnvSource`], so tests can exercise the `SHEETS_API_URL` override via
    /// `MapEnv` without mutating the process environment.
    pub(crate) fn from_drive_client_with(
        env: &impl EnvSource,
        drive: &DriveClient,
    ) -> Result<Self> {
        let base_url = Self::resolve_base_url(env);
        Ok(Self {
            inner: drive.transport().derived(&base_url, SERVICE_TAG),
        })
    }

    fn resolve_base_url(env: &impl EnvSource) -> String {
        env.var(SHEETS_API_URL)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string())
    }

    /// Returns the API base URL (without trailing slash).
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.inner.base_url()
    }

    /// The shared transport, for the `SheetsApi` façade's requests.
    pub(in crate::drive) fn transport(&self) -> &GoogleApiClient {
        &self.inner
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::DriveGrantedScopes;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    fn drive_client() -> DriveClient {
        DriveClient::new("https://www.googleapis.com", &test_credentials()).unwrap()
    }

    #[test]
    fn derives_the_real_sheets_host_by_default() {
        // Via a fresh MapEnv, not the real SystemEnv — a stray
        // SHEETS_API_URL in the process environment must not make this
        // flaky (mirrors the Drive/Gmail precedent).
        let sheets = SheetsClient::from_drive_client_with(&MapEnv::new(), &drive_client()).unwrap();
        assert_eq!(sheets.base_url(), "https://sheets.googleapis.com");
    }

    #[test]
    fn honours_the_sheets_api_url_override() {
        let env = MapEnv::new().with(SHEETS_API_URL, "http://127.0.0.1:9/sheets");
        let sheets = SheetsClient::from_drive_client_with(&env, &drive_client()).unwrap();
        assert_eq!(sheets.base_url(), "http://127.0.0.1:9/sheets");
    }

    #[test]
    fn ignores_an_empty_sheets_api_url_override() {
        let env = MapEnv::new().with(SHEETS_API_URL, "");
        let sheets = SheetsClient::from_drive_client_with(&env, &drive_client()).unwrap();
        assert_eq!(sheets.base_url(), "https://sheets.googleapis.com");
    }

    #[test]
    fn deriving_does_not_disturb_the_drive_client_host() {
        let drive = drive_client();
        let _sheets = SheetsClient::from_drive_client_with(&MapEnv::new(), &drive).unwrap();
        assert_eq!(drive.base_url(), "https://www.googleapis.com");
    }

    #[test]
    fn new_strips_a_trailing_slash() {
        let sheets =
            SheetsClient::new("https://sheets.googleapis.com/", &test_credentials()).unwrap();
        assert_eq!(sheets.base_url(), "https://sheets.googleapis.com");
    }

    #[test]
    fn debug_never_mentions_the_session_or_a_secret() {
        let sheets = SheetsClient::from_drive_client_with(&MapEnv::new(), &drive_client()).unwrap();
        let debug = format!("{sheets:?}");
        assert!(debug.contains("SheetsClient"));
        assert!(!debug.contains("session"));
        assert!(!debug.contains("secret-1"));
        assert!(!debug.contains("refresh-1"));
    }
}
