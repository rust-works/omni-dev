//! Bridge-driven harvesters.
//!
//! A *harvester* pages a logged-in browser session's **own** data out through
//! the bridge control plane, reusing the same `POST /__bridge/request` dispatch
//! path as `bridge request` (see [`crate::browser::client::BridgeClient`]).
//!
//! These drive **reverse-engineered, undocumented** site internals and are
//! therefore **best-effort**: they can break whenever the target site changes
//! its query ids, page structure, or response shape. Every harvester
//! re-discovers the volatile pieces (persisted-query ids, provider flags) on
//! each run rather than hardcoding them, and fails with a staged, actionable
//! error naming the step that drifted instead of panicking.
pub mod facebook;
