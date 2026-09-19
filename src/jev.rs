//! TypeSafe Jev "System One" API client.
//!
//! Jev takes unstructured program state and returns typed, probabilistic
//! judgments (pick-one, rate-on-a-scale, yes/no-probability) in a single
//! request. It is deliberately **not** an [`crate::claude::backend::AiBackend`]
//! — no chat messages, no streaming, no model-registry token limits, no
//! structured-output negotiation — so it lives in its own module rather than
//! under [`crate::claude`], mirroring the standalone [`crate::datadog`]
//! client.
//!
//! - [`client`] — the `reqwest` client and its single `POST /v1/systemone` call.
//! - [`config`] — credential and base-URL resolution.
//! - [`error`] — the [`error::JevError`] domain error type.
//! - [`protocol`] — the request/response wire types.

pub mod client;
pub mod config;
pub mod error;
pub mod protocol;
