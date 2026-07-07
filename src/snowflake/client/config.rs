//! Configuration for the clean-room Snowflake client.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use crate::utils::secret::Secret;

/// Default per-request HTTP timeout for Snowflake REST calls.
///
/// Generous so the query-request long-poll (the server holds it open until
/// results are ready or its synchronous window elapses, then returns an "in
/// progress" code) is not cut short; long-running queries are bounded by
/// `query_timeout` instead.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(120);

/// Default overall deadline for one query, including async-result polling.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(3600);

/// How to open the SSO URL during external-browser authentication.
#[derive(Clone, Debug, Default)]
pub enum BrowserLaunch {
    /// Open with the OS default handler (`open` / `xdg-open` / `start`).
    #[default]
    Auto,
    /// Run a custom command; `{url}` (or a trailing arg) receives the SSO URL.
    /// Use this to target a specific Chrome profile in a new window, e.g.
    /// `Google Chrome --profile-directory=Profile 1 --new-window {url}`.
    Command(Vec<String>),
    /// Do not open a browser; the SSO URL is logged for manual opening.
    Manual,
}

/// External-browser SSO settings.
#[derive(Clone, Debug)]
pub struct BrowserConfig {
    /// How to open the SSO URL.
    pub launch: BrowserLaunch,
    /// Bind address for the localhost callback listener.
    pub callback_addr: IpAddr,
    /// Bind port for the callback listener (`0` = OS-assigned ephemeral port).
    pub callback_port: u16,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            launch: BrowserLaunch::Auto,
            callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            callback_port: 0,
        }
    }
}

/// Key-pair (RS256 JWT) authentication settings.
#[derive(Clone, Debug)]
pub struct KeyPairConfig {
    /// The RSA private key, PEM-encoded. Must be **unencrypted** PKCS#8
    /// (`-----BEGIN PRIVATE KEY-----`); encrypted keys are not yet supported.
    pub private_key_pem: Secret,
}

/// The authentication method used to establish a session.
#[derive(Clone, Debug)]
pub enum AuthMethod {
    /// External-browser SSO: opens a browser and waits on a localhost callback.
    ExternalBrowser(BrowserConfig),
    /// A Snowflake programmatic access token, presented in place of a password.
    /// Non-interactive; requires the user to be covered by a network policy.
    ProgrammaticAccessToken {
        /// The PAT secret.
        token: Secret,
    },
    /// Key-pair authentication: a locally-signed RS256 JWT. Non-interactive;
    /// requires the user's RSA public key to be registered with the account
    /// (`ALTER USER … SET RSA_PUBLIC_KEY = …`).
    KeyPairJwt(KeyPairConfig),
}

/// Everything needed to authenticate and run queries against one account/user.
#[derive(Clone, Debug)]
pub struct SnowflakeClientConfig {
    /// Snowflake account identifier.
    pub account: String,
    /// Login user.
    pub user: String,
    /// Authentication method.
    pub auth: AuthMethod,
    /// Default warehouse applied at login.
    pub warehouse: Option<String>,
    /// Default role applied at login.
    pub role: Option<String>,
    /// Default database applied at login.
    pub database: Option<String>,
    /// Default schema applied at login.
    pub schema: Option<String>,
    /// Override the API host (defaults to `<account>.snowflakecomputing.com`).
    pub host: Option<String>,
    /// Per-request HTTP timeout for REST calls (not the SSO callback wait).
    pub http_timeout: Duration,
    /// Overall deadline for one query (submit + async-result polling).
    pub query_timeout: Duration,
}

impl SnowflakeClientConfig {
    /// Builds a config for external-browser SSO with default browser settings.
    #[must_use]
    pub fn external_browser(account: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            user: user.into(),
            auth: AuthMethod::ExternalBrowser(BrowserConfig::default()),
            warehouse: None,
            role: None,
            database: None,
            schema: None,
            host: None,
            http_timeout: DEFAULT_HTTP_TIMEOUT,
            query_timeout: DEFAULT_QUERY_TIMEOUT,
        }
    }

    /// The API host for this account.
    ///
    /// Uses the `host` override when set; otherwise derives
    /// `<account>.snowflakecomputing.com` from the account identifier — lowercased
    /// and with underscores mapped to dashes (Snowflake's URL rule), while region/
    /// cloud dot-segments (e.g. `acct.us-east-1.aws`) are preserved.
    #[must_use]
    pub fn api_host(&self) -> String {
        if let Some(host) = &self.host {
            return host.clone();
        }
        let account = self.account.trim().to_lowercase().replace('_', "-");
        format!("{account}.snowflakecomputing.com")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn api_host_maps_account_identifiers() {
        let host =
            |account: &str| SnowflakeClientConfig::external_browser(account, "me").api_host();
        // Lowercased.
        assert_eq!(host("MyAcct"), "myacct.snowflakecomputing.com");
        // Underscores → dashes (Snowflake URL rule).
        assert_eq!(host("my_org-acct"), "my-org-acct.snowflakecomputing.com");
        // Region/cloud dot-segments preserved.
        assert_eq!(
            host("xy12345.us-east-1.aws"),
            "xy12345.us-east-1.aws.snowflakecomputing.com"
        );
    }

    #[test]
    fn api_host_override_wins() {
        let cfg = SnowflakeClientConfig {
            host: Some("acct.privatelink.snowflakecomputing.com".to_string()),
            ..SnowflakeClientConfig::external_browser("MyAcct", "me")
        };
        assert_eq!(cfg.api_host(), "acct.privatelink.snowflakecomputing.com");
    }

    #[test]
    fn default_http_timeout_is_set() {
        let cfg = SnowflakeClientConfig::external_browser("a", "b");
        assert_eq!(cfg.http_timeout, DEFAULT_HTTP_TIMEOUT);
    }
}
