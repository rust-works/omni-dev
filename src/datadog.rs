//! Datadog API integration: read-only HTTP client and helpers.
//!
//! Provides a thin `reqwest` wrapper around the Datadog REST APIs for
//! metrics, monitors, dashboards, and logs. Phase 1 covers authentication
//! only; endpoint families land in subsequent slices.

pub mod auth;
pub mod client;
pub mod dashboards_api;
pub mod error;
pub mod events_api;
pub mod logs_api;
pub mod metrics_api;
pub mod monitors_api;
pub mod time;
pub mod types;

#[cfg(test)]
pub(crate) mod test_support;
