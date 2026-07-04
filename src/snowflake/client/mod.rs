//! A clean-room Snowflake client: external-browser SSO + the v1 query endpoint,
//! with session-token renewal and keep-alive heartbeats.
//!
//! Implemented from Snowflake's documented REST protocol — no third-party
//! connector. Unlike `snowflake-connector-rs`, it captures the **master token**
//! at login, so the session token can be renewed
//! ([`SnowflakeSession::renew`]) and kept alive
//! ([`SnowflakeSession::heartbeat`]) — letting the daemon authenticate once and
//! stay live indefinitely instead of dying when the ~1h session token lapses.
//!
//! Verification status: the offline-decodable paths (result parsing, row→JSON,
//! request shaping, callback parsing) are unit-tested; the live
//! SSO/login/query/renew paths follow the documented protocol but need a real
//! account to verify. Result sets that span external chunks are downloaded and
//! appended transparently (gzip JSON chunks after the inline rows); only the
//! Arrow result format is not yet supported (a clear [`Error::Unsupported`] is
//! returned).

pub mod config;
pub mod error;
pub mod row;

mod auth;
mod session;
mod transport;

use std::sync::Arc;

pub use config::{AuthMethod, BrowserConfig, BrowserLaunch, SnowflakeClientConfig};
pub use error::{Error, Result};
pub use row::{rows_to_payload, value_to_json, Column, Row};
pub use session::SnowflakeSession;

use transport::Transport;

/// A clean-room Snowflake client bound to one account/user configuration.
pub struct SnowflakeClient {
    transport: Arc<Transport>,
    config: SnowflakeClientConfig,
}

impl SnowflakeClient {
    /// Builds a client. Cheap — resolves the API host; does not authenticate.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] if the resolved host is invalid, or a transport build
    /// failure.
    pub fn new(config: SnowflakeClientConfig) -> Result<Self> {
        let transport = Arc::new(Transport::new(&config.api_host(), config.http_timeout)?);
        Ok(Self { transport, config })
    }

    /// The configuration this client was built with.
    #[must_use]
    pub fn config(&self) -> &SnowflakeClientConfig {
        &self.config
    }

    /// Authenticates and returns a live session. For external-browser SSO this
    /// opens a browser once.
    ///
    /// # Errors
    ///
    /// [`Error::Auth`] when the SSO flow fails, or a transport/server error.
    pub async fn create_session(&self) -> Result<SnowflakeSession> {
        let AuthMethod::ExternalBrowser(browser) = &self.config.auth;
        let tokens = auth::external_browser_login(&self.transport, &self.config, browser).await?;
        Ok(SnowflakeSession::new(
            Arc::clone(&self.transport),
            tokens,
            self.config.query_timeout,
        ))
    }
}

/// Builds a session whose transport targets `base_url` with placeholder tokens,
/// so the engine's query orchestration can be exercised offline against a mock
/// server without going through the (live-only) browser SSO flow.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) fn test_session(base_url: &str, query_timeout: std::time::Duration) -> SnowflakeSession {
    use std::time::Duration;
    let url = url::Url::parse(base_url).unwrap().join("/").unwrap();
    let transport =
        Arc::new(transport::Transport::with_base_url(url, Duration::from_secs(5)).unwrap());
    SnowflakeSession::new(
        transport,
        session::LoginTokens {
            session_token: "test-sess".into(),
            master_token: "test-mast".into(),
            session_validity_secs: 3600,
            master_validity_secs: 14_400,
        },
        query_timeout,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn new_builds_a_client_and_exposes_its_config() {
        let client = SnowflakeClient::new(SnowflakeClientConfig::external_browser("MyAcct", "me"))
            .expect("a valid account resolves to a valid host");
        assert_eq!(client.config().account, "MyAcct");
        assert_eq!(client.config().user, "me");
        assert_eq!(client.config().api_host(), "myacct.snowflakecomputing.com");
    }

    #[test]
    fn new_rejects_an_invalid_host_override() {
        let config = SnowflakeClientConfig {
            // An unterminated IPv6 literal can never parse as a URL host.
            host: Some("[".to_string()),
            ..SnowflakeClientConfig::external_browser("acct", "me")
        };
        assert!(matches!(
            SnowflakeClient::new(config),
            Err(Error::Protocol(_))
        ));
    }
}
