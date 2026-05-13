//! `MockTranscriber` — emits a caller-supplied event script.
//!
//! Stand-in for a real ASR backend while #801's inference choice is being
//! finalised. Behaviour:
//!
//! - Drains the [`AudioInput`] to compute the stream's total duration.
//! - Emits one [`TranscriptEvent::Final`] per [`MockSegment`] in the
//!   configured script, in order, with a fresh `event_id` per segment.
//! - Emits a terminal [`TranscriptEvent::Endpoint`] with
//!   `kind = StreamEnd` and `at = total_duration`.
//!
//! All emitted finals have `revisable = false`, matching #801's contract
//! for batch backends.
//!
//! The `event_id` source is pluggable via [`UlidRng`] so tests can pin
//! down deterministic ULIDs for snapshotting. Production uses
//! [`SystemUlidRng`] (`Ulid::new()`); tests use [`CountingUlidRng`] from
//! the same module.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use ulid::Ulid;

use crate::voice::transcriber::{
    AudioInput, EndpointKind, EventId, EventStream, Transcriber, TranscriptEvent,
};

/// One Final event the mock will emit. Built before `transcribe` runs;
/// the mock copies these into events at transcribe time with ULIDs minted
/// from its [`UlidRng`].
#[derive(Debug, Clone)]
pub struct MockSegment {
    /// Text the mock will emit for this segment.
    pub text: String,
    /// Stream-relative start time for the emitted `Final`.
    pub start: Duration,
    /// Stream-relative end time for the emitted `Final`.
    pub end: Duration,
    /// Confidence in `[0.0, 1.0]` to attach to the emitted `Final`.
    pub confidence: f32,
}

/// Source of [`EventId`]s. Pluggable so tests can pin ULIDs while
/// production uses real entropy.
pub trait UlidRng: Send + Sync {
    /// Returns the next ULID. Each call must produce a value strictly
    /// greater than the previous one to preserve event-id monotonicity.
    fn next_ulid(&mut self) -> Ulid;
}

/// Production RNG: defers to `Ulid::new()`.
#[derive(Debug, Default)]
pub struct SystemUlidRng;

impl UlidRng for SystemUlidRng {
    fn next_ulid(&mut self) -> Ulid {
        Ulid::new()
    }
}

/// Deterministic RNG for tests.
///
/// Returns `Ulid::from_parts(0, counter)` for an increasing counter
/// starting at 1. The encoded form is lexicographically ordered, so a
/// sequence of ULIDs from this RNG is monotonically increasing — exactly
/// the property snapshot tests rely on.
#[derive(Debug, Default)]
pub struct CountingUlidRng {
    counter: u128,
}

impl CountingUlidRng {
    /// Builds a new counting RNG starting at zero — the first ULID it
    /// returns has random bits `1`, the second `2`, and so on.
    pub fn new() -> Self {
        Self { counter: 0 }
    }
}

impl UlidRng for CountingUlidRng {
    fn next_ulid(&mut self) -> Ulid {
        self.counter += 1;
        Ulid::from_parts(0, self.counter)
    }
}

/// A canned-script Transcriber used as a placeholder until a real ASR
/// backend lands.
pub struct MockTranscriber {
    script: Vec<MockSegment>,
    rng: Mutex<Box<dyn UlidRng>>,
}

impl MockTranscriber {
    /// Builds a mock with a real-entropy ULID source.
    pub fn new(script: Vec<MockSegment>) -> Self {
        Self {
            script,
            rng: Mutex::new(Box::new(SystemUlidRng)),
        }
    }

    /// Test-friendly constructor: caller supplies the RNG. Use
    /// [`CountingUlidRng`] for snapshot stability.
    pub fn with_rng(script: Vec<MockSegment>, rng: Box<dyn UlidRng>) -> Self {
        Self {
            script,
            rng: Mutex::new(rng),
        }
    }

    /// The script the factory uses when no caller-side script is supplied
    /// (i.e. when this backend is picked via `OMNI_DEV_VOICE_BACKEND=mock`).
    /// Deliberately bland placeholder text so consumers never confuse mock
    /// output with real transcription.
    pub fn default_script() -> Vec<MockSegment> {
        vec![
            MockSegment {
                text: "[mock transcriber] segment 1".to_string(),
                start: Duration::from_millis(0),
                end: Duration::from_secs(2),
                confidence: 1.0,
            },
            MockSegment {
                text: "[mock transcriber] segment 2".to_string(),
                start: Duration::from_secs(2),
                end: Duration::from_secs(5),
                confidence: 1.0,
            },
        ]
    }
}

impl Transcriber for MockTranscriber {
    fn transcribe(&self, mut audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>> {
        // Drain audio to determine total duration. At 16 kHz mono, one
        // sample = 1/16000 s.
        let mut total_samples: usize = 0;
        while let Some(chunk) = audio.next_chunk() {
            total_samples = total_samples.saturating_add(chunk.len());
        }
        #[allow(clippy::cast_precision_loss)]
        let total_duration = Duration::from_secs_f64(total_samples as f64 / 16_000.0);

        let mut events: Vec<Result<TranscriptEvent>> = Vec::with_capacity(self.script.len() + 1);
        for seg in &self.script {
            let event_id: EventId = {
                let mut rng = self
                    .rng
                    .lock()
                    .map_err(|e| anyhow::anyhow!("MockTranscriber RNG mutex poisoned: {e}"))?;
                rng.next_ulid()
            };
            events.push(Ok(TranscriptEvent::Final {
                event_id,
                text: seg.text.clone(),
                start: seg.start,
                end: seg.end,
                confidence: seg.confidence,
                words: None,
                speaker: None,
                revisable: false,
            }));
        }
        events.push(Ok(TranscriptEvent::Endpoint {
            at: total_duration,
            kind: EndpointKind::StreamEnd,
        }));
        Ok(Box::new(events.into_iter()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::transcriber::VecAudioInput;

    fn run(transcriber: &MockTranscriber, samples: Vec<i16>) -> Vec<TranscriptEvent> {
        let input = VecAudioInput::from_samples(samples, 1024);
        transcriber
            .transcribe(Box::new(input))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn three_segment_script() -> Vec<MockSegment> {
        vec![
            MockSegment {
                text: "alpha".into(),
                start: Duration::from_millis(0),
                end: Duration::from_millis(100),
                confidence: 0.9,
            },
            MockSegment {
                text: "beta".into(),
                start: Duration::from_millis(100),
                end: Duration::from_millis(200),
                confidence: 0.95,
            },
            MockSegment {
                text: "gamma".into(),
                start: Duration::from_millis(200),
                end: Duration::from_millis(300),
                confidence: 0.99,
            },
        ]
    }

    #[test]
    fn finals_precede_terminal_endpoint() {
        let t = MockTranscriber::with_rng(three_segment_script(), Box::new(CountingUlidRng::new()));
        let events = run(&t, vec![0; 16_000]); // 1 s of silence
        assert_eq!(events.len(), 4);
        for (i, e) in events.iter().take(3).enumerate() {
            assert!(
                matches!(e, TranscriptEvent::Final { .. }),
                "event {i} should be Final, got {e:?}"
            );
        }
        match &events[3] {
            TranscriptEvent::Endpoint { kind, .. } => {
                assert_eq!(*kind, EndpointKind::StreamEnd);
            }
            other => panic!("expected terminal Endpoint, got {other:?}"),
        }
    }

    #[test]
    fn every_final_has_revisable_false() {
        let t = MockTranscriber::with_rng(three_segment_script(), Box::new(CountingUlidRng::new()));
        let events = run(&t, vec![0; 16_000]);
        for e in &events {
            if let TranscriptEvent::Final { revisable, .. } = e {
                assert!(!revisable, "batch finals must not be revisable");
            }
        }
    }

    #[test]
    fn ulids_are_monotonically_increasing() {
        let t = MockTranscriber::with_rng(three_segment_script(), Box::new(CountingUlidRng::new()));
        let events = run(&t, vec![0; 16_000]);
        let ids: Vec<EventId> = events
            .iter()
            .filter_map(|e| match e {
                TranscriptEvent::Final { event_id, .. } => Some(*event_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 3);
        for pair in ids.windows(2) {
            assert!(
                pair[0] < pair[1],
                "ULIDs should be strictly increasing: {:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn counting_ulid_rng_is_deterministic_across_runs() {
        let s1 =
            MockTranscriber::with_rng(three_segment_script(), Box::new(CountingUlidRng::new()))
                .transcribe(Box::new(VecAudioInput::from_samples(vec![0; 16_000], 1024)))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
        let s2 =
            MockTranscriber::with_rng(three_segment_script(), Box::new(CountingUlidRng::new()))
                .transcribe(Box::new(VecAudioInput::from_samples(vec![0; 16_000], 1024)))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
        assert_eq!(s1, s2);
    }

    #[test]
    fn endpoint_at_matches_total_audio_duration() {
        let t = MockTranscriber::with_rng(vec![], Box::new(CountingUlidRng::new()));
        // 32000 samples at 16 kHz = 2 s.
        let events = run(&t, vec![0; 32_000]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            TranscriptEvent::Endpoint { at, .. } => {
                assert!(
                    (at.as_secs_f64() - 2.0).abs() < 1e-9,
                    "expected 2 s endpoint, got {at:?}"
                );
            }
            other => panic!("expected Endpoint, got {other:?}"),
        }
    }

    #[test]
    fn empty_script_still_emits_endpoint() {
        let t = MockTranscriber::with_rng(vec![], Box::new(CountingUlidRng::new()));
        let events = run(&t, vec![0; 1_600]); // 0.1 s
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TranscriptEvent::Endpoint { .. }));
    }

    #[test]
    fn system_ulid_rng_produces_unique_values() {
        let mut rng = SystemUlidRng;
        let a = rng.next_ulid();
        let b = rng.next_ulid();
        // Different ULIDs minted milliseconds apart should not collide.
        assert_ne!(a, b);
    }

    #[test]
    fn default_script_yields_two_segments() {
        let script = MockTranscriber::default_script();
        assert_eq!(script.len(), 2);
        assert!(script[0].text.starts_with("[mock"));
    }

    #[test]
    fn poisoned_rng_mutex_errors_cleanly() {
        // A panic inside `next_ulid` unwinds while the RNG mutex guard is
        // held, marking the mutex as poisoned. The next `transcribe` call
        // must surface that as a clean error, not propagate the poison.
        // This pins the `map_err(|e| anyhow!("…poisoned…"))` arm.
        use std::panic::{self, AssertUnwindSafe};

        struct PanickingRng;
        impl UlidRng for PanickingRng {
            fn next_ulid(&mut self) -> Ulid {
                panic!("test-induced panic");
            }
        }

        let script = vec![MockSegment {
            text: "x".into(),
            start: Duration::from_millis(0),
            end: Duration::from_millis(100),
            confidence: 1.0,
        }];
        let t = MockTranscriber::with_rng(script, Box::new(PanickingRng));

        let first = panic::catch_unwind(AssertUnwindSafe(|| {
            let input = VecAudioInput::from_samples(vec![0; 1_600], 1024);
            let _ = t.transcribe(Box::new(input));
        }));
        assert!(first.is_err(), "first call should panic from PanickingRng");

        let input = VecAudioInput::from_samples(vec![0; 1_600], 1024);
        let Err(err) = t.transcribe(Box::new(input)) else {
            panic!("expected poisoned-mutex error from transcribe");
        };
        assert!(
            err.to_string().contains("poisoned"),
            "expected poisoned mutex error, got: {err}"
        );
    }
}
