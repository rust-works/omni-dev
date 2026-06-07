//! Streaming token→event segmentation ([ADR-0038](../../docs/adrs/adr-0038.md)).
//!
//! [`StreamSegmenter`] turns a streaming backend's incremental output — drained
//! token strings plus per-100 ms-window silence classifications from
//! [`IdleDetector`](crate::voice::idle::IdleDetector) — into the project's
//! `Partial` / `Final` / `Endpoint` events. It is **engine-agnostic** (no FFI,
//! no feature gate) so the segmentation heuristic is unit-tested in the default
//! suite, independent of any real ASR engine. The streaming `VoxtralBackend`
//! (#933 Phase 6) drives it; future streaming backends (#806) can reuse it.
//!
//! Boundaries:
//! - tokens arrive → accumulate into the current utterance → emit `Partial`;
//! - a silence gap (≥ `silence_gap_windows` consecutive silent windows while an
//!   utterance is pending) → `Final { revisable: true }` + `Endpoint{SilenceGap}`,
//!   then reset for the next utterance;
//! - end of audio → `Final { revisable: false }` + `Endpoint{StreamEnd}`.

use std::time::Duration;

use crate::voice::det::{SystemUlidRng, UlidRng};
use crate::voice::idle::WindowClass;
use crate::voice::transcriber::{EndpointKind, TranscriptEvent};

/// Accumulates streaming tokens and silence observations into transcript
/// events. See the module docs for the boundary rules.
pub struct StreamSegmenter {
    /// Text accumulated for the current (not-yet-committed) utterance.
    utterance: String,
    /// Stream-relative start time of the current utterance.
    utterance_start: Duration,
    /// Consecutive silent 100 ms windows observed since the last voiced window.
    consec_silent: u32,
    /// Silent-window streak that triggers a silence-gap commit.
    silence_gap_windows: u32,
    /// ULID source for `Final` event ids (pluggable for deterministic tests).
    rng: Box<dyn UlidRng>,
}

impl StreamSegmenter {
    /// Builds a segmenter that commits an utterance after `silence_gap_windows`
    /// consecutive silent 100 ms windows, using a real-entropy ULID source.
    #[must_use]
    pub fn new(silence_gap_windows: u32) -> Self {
        Self::with_rng(silence_gap_windows, Box::new(SystemUlidRng))
    }

    /// Test-friendly constructor: caller supplies the ULID source (use
    /// [`crate::voice::det::CountingUlidRng`] for determinism).
    #[must_use]
    pub fn with_rng(silence_gap_windows: u32, rng: Box<dyn UlidRng>) -> Self {
        Self {
            utterance: String::new(),
            utterance_start: Duration::ZERO,
            consec_silent: 0,
            silence_gap_windows,
            rng,
        }
    }

    /// Appends freshly-drained `tokens` to the current utterance and returns a
    /// `Partial` if (after appending) there is pending text. Returns `None`
    /// when `tokens` is empty (nothing changed) or the utterance is still
    /// blank.
    pub fn push_tokens(&mut self, tokens: &[String], now: Duration) -> Option<TranscriptEvent> {
        if tokens.is_empty() {
            return None;
        }
        for token in tokens {
            self.utterance.push_str(token);
        }
        let text = self.utterance.trim();
        if text.is_empty() {
            return None;
        }
        Some(TranscriptEvent::Partial {
            text: text.to_string(),
            start: self.utterance_start,
            end: now,
            words: None,
            speaker: None,
        })
    }

    /// Folds window classifications into the silence counter. Returns `true`
    /// when a silence-gap boundary is reached *and* an utterance is pending —
    /// the caller should then flush the engine and call
    /// [`Self::commit_silence_gap`]. Voiced windows reset the streak.
    pub fn observe_silence(&mut self, classes: &[WindowClass]) -> bool {
        for class in classes {
            match class {
                WindowClass::Silent => self.consec_silent += 1,
                WindowClass::Voiced => self.consec_silent = 0,
            }
        }
        self.consec_silent >= self.silence_gap_windows && !self.utterance.trim().is_empty()
    }

    /// Commits the current utterance as a revisable `Final` followed by an
    /// `Endpoint{SilenceGap}`, after appending any post-flush `tokens`. Resets
    /// the utterance, advances the next utterance's start to `now`, and clears
    /// the silence streak. Emits no `Final` if the utterance is blank.
    pub fn commit_silence_gap(
        &mut self,
        tokens: &[String],
        now: Duration,
        confidence: f32,
    ) -> Vec<TranscriptEvent> {
        for token in tokens {
            self.utterance.push_str(token);
        }
        let mut events = Vec::new();
        let text = self.utterance.trim();
        if !text.is_empty() {
            events.push(TranscriptEvent::Final {
                event_id: self.rng.next_ulid(),
                text: text.to_string(),
                start: self.utterance_start,
                end: now,
                confidence,
                words: None,
                speaker: None,
                revisable: true,
            });
            events.push(TranscriptEvent::Endpoint {
                at: now,
                kind: EndpointKind::SilenceGap,
            });
        }
        self.reset_to(now);
        events
    }

    /// Commits the current utterance as a non-revisable `Final` (if any) and a
    /// terminal `Endpoint{StreamEnd}` at end of audio, after appending any
    /// post-finish `tokens`. The `StreamEnd` endpoint is always emitted.
    pub fn commit_end(
        &mut self,
        tokens: &[String],
        now: Duration,
        confidence: f32,
    ) -> Vec<TranscriptEvent> {
        for token in tokens {
            self.utterance.push_str(token);
        }
        let mut events = Vec::new();
        let text = self.utterance.trim();
        if !text.is_empty() {
            events.push(TranscriptEvent::Final {
                event_id: self.rng.next_ulid(),
                text: text.to_string(),
                start: self.utterance_start,
                end: now,
                confidence,
                words: None,
                speaker: None,
                revisable: false,
            });
        }
        events.push(TranscriptEvent::Endpoint {
            at: now,
            kind: EndpointKind::StreamEnd,
        });
        self.reset_to(now);
        events
    }

    fn reset_to(&mut self, now: Duration) {
        self.utterance.clear();
        self.utterance_start = now;
        self.consec_silent = 0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::det::CountingUlidRng;

    fn seg(gap: u32) -> StreamSegmenter {
        StreamSegmenter::with_rng(gap, Box::new(CountingUlidRng::new()))
    }

    fn s(text: &str) -> Vec<String> {
        vec![text.to_string()]
    }

    #[test]
    fn push_tokens_accumulates_and_emits_partial() {
        let mut seg = seg(7);
        assert!(seg.push_tokens(&[], Duration::from_millis(100)).is_none());
        let p1 = seg
            .push_tokens(&s("the "), Duration::from_millis(200))
            .unwrap();
        let p2 = seg
            .push_tokens(&s("quick"), Duration::from_millis(300))
            .unwrap();
        match p1 {
            TranscriptEvent::Partial {
                text, start, end, ..
            } => {
                assert_eq!(text, "the");
                assert_eq!(start, Duration::ZERO);
                assert_eq!(end, Duration::from_millis(200));
            }
            other => panic!("expected Partial, got {other:?}"),
        }
        match p2 {
            TranscriptEvent::Partial { text, .. } => assert_eq!(text, "the quick"),
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    #[test]
    fn observe_silence_fires_at_threshold_only_with_pending_text() {
        let mut seg = seg(3);
        // Leading silence with no utterance never fires; the trailing voiced
        // window resets the streak (the realistic flow — speech produces voiced
        // windows before tokens arrive).
        assert!(!seg.observe_silence(&[
            WindowClass::Silent,
            WindowClass::Silent,
            WindowClass::Silent,
            WindowClass::Voiced,
        ]));
        seg.push_tokens(&s("hello"), Duration::from_millis(100));
        // Below threshold (2 < 3).
        assert!(!seg.observe_silence(&[WindowClass::Silent, WindowClass::Silent]));
        // Reaches the threshold (3) with text pending → fires.
        assert!(seg.observe_silence(&[WindowClass::Silent]));
    }

    #[test]
    fn voiced_window_resets_silence_streak() {
        let mut seg = seg(3);
        seg.push_tokens(&s("hi"), Duration::from_millis(100));
        seg.observe_silence(&[WindowClass::Silent, WindowClass::Silent]);
        // A voiced window resets the streak; two silents after is < 3.
        assert!(!seg.observe_silence(&[
            WindowClass::Voiced,
            WindowClass::Silent,
            WindowClass::Silent
        ]));
    }

    #[test]
    fn commit_silence_gap_emits_revisable_final_plus_silencegap_and_resets() {
        let mut seg = seg(2);
        seg.push_tokens(&s("first utterance"), Duration::from_millis(500));
        let events = seg.commit_silence_gap(&[], Duration::from_secs(1), 1.0);
        assert_eq!(events.len(), 2);
        match &events[0] {
            TranscriptEvent::Final {
                text,
                revisable,
                start,
                end,
                ..
            } => {
                assert_eq!(text, "first utterance");
                assert!(*revisable, "silence-gap finals are revisable");
                assert_eq!(*start, Duration::ZERO);
                assert_eq!(*end, Duration::from_secs(1));
            }
            other => panic!("expected Final, got {other:?}"),
        }
        assert!(matches!(
            events[1],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::SilenceGap,
                ..
            }
        ));
        // After reset, the next utterance starts at the commit time.
        let p = seg
            .push_tokens(&s("second"), Duration::from_millis(1500))
            .unwrap();
        match p {
            TranscriptEvent::Partial { text, start, .. } => {
                assert_eq!(text, "second");
                assert_eq!(start, Duration::from_secs(1));
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    #[test]
    fn commit_end_emits_nonrevisable_final_and_streamend() {
        let mut seg = seg(7);
        seg.push_tokens(&s("final words"), Duration::from_millis(900));
        let events = seg.commit_end(&[], Duration::from_secs(1), 0.9);
        assert_eq!(events.len(), 2);
        match &events[0] {
            TranscriptEvent::Final {
                text,
                revisable,
                confidence,
                ..
            } => {
                assert_eq!(text, "final words");
                assert!(!*revisable, "end-of-stream finals are not revisable");
                assert!((*confidence - 0.9).abs() < 1e-6);
            }
            other => panic!("expected Final, got {other:?}"),
        }
        assert!(matches!(
            events[1],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::StreamEnd,
                ..
            }
        ));
    }

    #[test]
    fn commit_end_with_no_text_still_emits_streamend() {
        let mut seg = seg(7);
        let events = seg.commit_end(&[], Duration::from_secs(2), 1.0);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::StreamEnd,
                ..
            }
        ));
    }

    #[test]
    fn post_flush_tokens_are_appended_before_commit() {
        let mut seg = seg(2);
        seg.push_tokens(&s("hello "), Duration::from_millis(500));
        // The flush drained a trailing token that belongs to this utterance.
        let events = seg.commit_silence_gap(&s("world"), Duration::from_secs(1), 1.0);
        match &events[0] {
            TranscriptEvent::Final { text, .. } => assert_eq!(text, "hello world"),
            other => panic!("expected Final, got {other:?}"),
        }
    }
}
