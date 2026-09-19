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
//! - [`input`] — shared input-text truncation, used by `route` and `verify`.
//! - [`protocol`] — the request/response wire types.
//! - [`route`] — stage routing of issues to model classes (`ai jev route`).
//! - [`verify`] — decision-comment verification (`ai jev verify-decision`).

pub mod client;
pub mod config;
pub mod error;
pub mod input;
pub mod protocol;
pub mod route;
pub mod verify;
