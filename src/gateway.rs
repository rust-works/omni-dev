//! Anthropic Messages gateway in front of OpenAI-protocol upstreams (#2226).
//!
//! It serves the Anthropic protocol to clients such as Claude Code and the
//! Anthropic SDKs, backed by an OpenAI-protocol upstream (OpenAI, Ollama,
//! LM Studio, vLLM, OpenRouter, Fireworks) or, in passthrough mode, an
//! Anthropic-compatible one.
//!
//! The inbound direction is the point: omni-dev's AI backends *call* these
//! upstreams, the gateway lets other tools reach them through the Anthropic API.
//! The engine is split like the browser bridge and Snowflake services:
//!
//! - wire types and the request policy: [`anthropic`], [`policy`];
//! - translation: [`lower`] (conversation), [`chat`] / [`responses`] (request
//!   and reply), [`completion`] (reply → Anthropic), [`stream`] and the two
//!   stream parsers (SSE re-framing), [`passthrough`];
//! - support: [`config`], [`models`], [`flavor`], [`signature`],
//!   [`think_tags`], [`sse`], [`tokens`], [`error`];
//! - the HTTP surface: [`server`].
//!
//! The standalone `omni-dev ai gateway serve` and the daemon's
//! `anthropic-gateway` service host the same [`server::GatewayServer`]. The
//! security model (loopback bind, a token on every request, Host and Origin
//! guards) is [ADR-0098](../../docs/adrs/adr-0098.md); the operator guide is
//! `docs/anthropic-gateway.md`.

pub(crate) mod anthropic;
pub(crate) mod chat;
pub(crate) mod chat_stream;
pub(crate) mod completion;
pub mod config;
#[cfg(test)]
mod e2e_tests;
pub(crate) mod error;
pub mod flavor;
pub(crate) mod lower;
pub mod models;
pub(crate) mod passthrough;
pub mod policy;
pub(crate) mod responses;
pub(crate) mod responses_stream;
pub mod server;
pub(crate) mod signature;
pub(crate) mod sse;
pub(crate) mod stream;
pub(crate) mod think_tags;
pub(crate) mod tokens;

pub use config::{ConfigOverrides, GatewayConfig, GatewayConfigError};
pub use flavor::{Flavor, UpstreamProtocol};
pub use server::{GatewayServer, StatsSnapshot};
