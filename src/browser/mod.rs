//! Browser bridge: drive HTTP requests through an authenticated browser tab.
//!
//! The bridge runs two planes — a WebSocket plane the browser connects to (via
//! a pasted DevTools snippet) and an HTTP control plane the operator drives —
//! joined by an `id`-keyed correlator. It deliberately lets a local process
//! borrow the browser's authenticated session (a confused-deputy by design), so
//! both planes are authenticated and default-closed. See
//! [docs/browser-bridge.md](../../docs/browser-bridge.md) and
//! [ADR-0036](../../docs/adrs/adr-0036.md).

pub mod auth;
pub mod bridge;
pub mod protocol;
pub mod snippet;

pub use bridge::{run, BridgeConfig};
