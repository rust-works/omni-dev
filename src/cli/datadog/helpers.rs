//! Shared helpers for Datadog CLI commands.

use anyhow::Result;

use crate::datadog::auth;
use crate::datadog::client::DatadogClient;

/// Creates an authenticated Datadog API client, returning the client and
/// the configured site identifier.
pub fn create_client() -> Result<(DatadogClient, String)> {
    let credentials = auth::load_credentials()?;
    let site = credentials.site.clone();
    let client = DatadogClient::from_credentials(&credentials)?;
    Ok((client, site))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use super::*;
    use crate::datadog::test_support::{with_empty_home, EnvGuard};

    #[test]
    fn create_client_reads_from_settings_file() {
        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        let omni_dir = dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        fs::write(
            omni_dir.join("settings.json"),
            r#"{"env": {
                "DATADOG_API_KEY": "api",
                "DATADOG_APP_KEY": "app",
                "DATADOG_SITE": "us5.datadoghq.com"
            }}"#,
        )
        .unwrap();

        let (client, site) = create_client().unwrap();
        assert_eq!(site, "us5.datadoghq.com");
        assert_eq!(client.base_url(), "https://api.us5.datadoghq.com");
    }

    #[test]
    fn create_client_errors_when_credentials_missing() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let err = create_client().unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }
}
