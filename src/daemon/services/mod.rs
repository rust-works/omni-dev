//! Built-in daemon services.
//!
//! Each submodule provides one [`DaemonService`](super::service::DaemonService)
//! implementation: [`bridge`] hosts the browser bridge and [`snowflake`] hosts
//! the Snowflake query engine (the real services); [`echo`] is a trivial test
//! service used by the framework's own tests.

pub mod bridge;
pub mod echo;
pub mod snowflake;
