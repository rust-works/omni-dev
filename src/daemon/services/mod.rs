//! Built-in daemon services.
//!
//! Each submodule provides one [`DaemonService`](super::service::DaemonService)
//! implementation. The browser bridge service joins this module in a later
//! change (#987); for now only the [`echo`] test service lives here.

pub mod echo;
