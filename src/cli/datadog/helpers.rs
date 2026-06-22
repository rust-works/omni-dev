//! Shared helpers for Datadog CLI commands.

use anyhow::Result;

use crate::datadog::auth;
use crate::datadog::client::DatadogClient;

/// Creates an authenticated Datadog API client, returning the client and
/// the configured site identifier.
pub fn create_client() -> Result<(DatadogClient, String)> {
    create_client_from(auth::load_credentials()?)
}

/// Builds a client from already-resolved credentials, returning the client
/// and the configured site.
///
/// The dependency-injection seam: commands resolve credentials via
/// [`create_client`] in production, while tests construct a
/// [`DatadogCredentials`](auth::DatadogCredentials) value (or a wiremock
/// client) directly and never touch the environment (issue #1030).
pub fn create_client_from(
    credentials: auth::DatadogCredentials,
) -> Result<(DatadogClient, String)> {
    let site = credentials.site.clone();
    let client = DatadogClient::from_credentials(&credentials)?;
    Ok((client, site))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::datadog::auth::DatadogCredentials;

    #[test]
    fn create_client_from_propagates_site() {
        // Credential resolution from env/settings is covered by
        // `load_credentials_with` tests; here we only verify the value-in seam
        // wires the site through. No env/HOME mutation, so no lock needed.
        let creds = DatadogCredentials {
            api_key: "api".to_string(),
            app_key: "app".to_string(),
            site: "us5.datadoghq.com".to_string(),
        };
        let (_client, site) = create_client_from(creds).unwrap();
        assert_eq!(site, "us5.datadoghq.com");
    }
}
