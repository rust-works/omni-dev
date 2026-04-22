//! Shared helpers for Datadog CLI commands.
//!
//! `create_client` is unused in slice 1 (the auth status command builds its
//! own client so it can report the site separately); it becomes live with
//! the first endpoint subcommand.
#![allow(dead_code)]

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
