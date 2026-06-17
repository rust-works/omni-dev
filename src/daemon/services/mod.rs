//! Built-in daemon services.
//!
//! Each submodule provides one [`DaemonService`](super::service::DaemonService)
//! implementation: [`bridge`] hosts the browser bridge (the real service);
//! [`echo`] is a trivial test service used by the framework's own tests.

pub mod bridge;
pub mod echo;
