//! JSONL event-log schema emitted by both spike prototypes.
//!
//! `wall_ms` is wall-clock elapsed since process start (simulated-clock
//! time in the spike harness). `audio_ms` is the simulated-clock time
//! of the most recently *pushed* audio sample at emit-time, which we
//! use as the Partial-latency reference point: `wall_ms - audio_ms`
//! approximates "how long after audio arrived did we emit?". See the
//! "Latency proxy honesty note" in SPIKE.md.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Partial {
        wall_ms: u128,
        audio_ms: u128,
        text: String,
    },
    Final {
        wall_ms: u128,
        audio_ms: u128,
        event_id: String,
        text: String,
        confidence: f32,
    },
    SilenceOnset {
        wall_ms: u128,
        audio_ms: u128,
    },
    Endpoint {
        wall_ms: u128,
        audio_ms: u128,
        kind: EndpointKind,
    },
    ModelLoaded {
        wall_ms: u128,
        load_ms: u128,
    },
    StreamEnd {
        wall_ms: u128,
        audio_ms: u128,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    SilenceGap,
    StreamEnd,
}

impl Event {
    pub fn wall_ms(&self) -> u128 {
        match self {
            Self::Partial { wall_ms, .. }
            | Self::Final { wall_ms, .. }
            | Self::SilenceOnset { wall_ms, .. }
            | Self::Endpoint { wall_ms, .. }
            | Self::ModelLoaded { wall_ms, .. }
            | Self::StreamEnd { wall_ms, .. } => *wall_ms,
        }
    }
}
