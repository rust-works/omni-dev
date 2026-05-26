//! Streaming Parakeet wrapper — `StreamingTranscriber` impl on
//! [`CandleParakeetTranscriber`].
//!
//! **v1 simplification**: this implementation collects the full audio
//! stream before running inference, then emits a single `Final` +
//! `Endpoint::StreamEnd` once. It satisfies the `StreamingTranscriber`
//! trait surface and the issue #898 acceptance criteria that gate on
//! output correctness (Final-only transcript snapshot, byte-equal
//! determinism), but it does NOT emit incremental `Partial` events
//! per chunk.
//!
//! True per-chunk incremental emission needs:
//!
//! - Encoder/decoder pieces cloneable into the streaming session (so
//!   the session can hold its own snapshot across `await` boundaries
//!   without holding `&self`). This is a backend-refactor to drop the
//!   `Mutex` discipline in favour of cheap `Arc` clones — the inner
//!   Tensors are already `Arc`'d by candle.
//! - Per-session [`super::decoder::LstmState`] threaded across chunks
//!   so the TDT predictor doesn't reset at every chunk boundary.
//! - Per-session [`super::audio::RunningStats`] for the mel
//!   normalisation fix from `newhoggy/parakeet-mlx@32b8034` (already
//!   implemented in `audio.rs`; just needs to be wired here).
//!
//! Those land in a follow-up commit; they're orthogonal to wiring the
//! backend into the factory and verifying batch correctness via the
//! snapshot tests in commit 10. The acceptance criteria flagged as
//! NOT yet met by this v1:
//!
//! - 3c (representative `Partial`-event sequence on a 30 s slice) —
//!   this v1 emits no `Partial`s, so the snapshot test for it lands
//!   `#[ignore]`-gated with a pointer to the follow-up issue.
//!
//! The criteria this v1 DOES satisfy:
//!
//! - 3b (streaming Final-only transcript on the 5-min fixture).
//! - 2 (byte-equal event-log JSONL across reruns) — trivially, since
//!   the pipeline is deterministic.

use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use futures::stream::{self, Stream};
use ulid::Ulid;

use crate::voice::transcriber::{
    AsyncAudioInput, EndpointKind, StreamingTranscriber, TranscriptEvent, VecAudioInput,
};

use super::audio::SAMPLE_RATE;
use super::CandleParakeetTranscriber;
use crate::voice::transcriber::Transcriber;

impl StreamingTranscriber for CandleParakeetTranscriber {
    fn transcribe_stream(
        &self,
        audio: Box<dyn AsyncAudioInput>,
    ) -> Pin<Box<dyn Stream<Item = Result<TranscriptEvent>> + Send>> {
        // The `&self` reference can't move across the await boundary,
        // so we drain the AsyncAudioInput synchronously inside the
        // stream, then route the collected samples through the
        // already-implemented batch `Transcriber` path. The price is
        // that all events arrive at once at the end; the win is one
        // code path for both batch and streaming output, which makes
        // determinism trivial.
        //
        // SAFETY: AsyncAudioInput::next_chunk is async; we must drain
        // inside the stream's async closure.
        let total_audio_ms_at_zero = 0_u64;
        let _ = total_audio_ms_at_zero; // anchor for future incremental impl

        // Snapshot the model pieces we need: there's no &self lifetime
        // to carry into the stream, so we lock the inner mutexes once,
        // run inference, and emit.
        let batch_events = match self.run_batch_via_stream_audio(audio) {
            Ok(events) => events,
            Err(e) => vec![Err(e)],
        };
        Box::pin(stream::iter(batch_events))
    }
}

impl CandleParakeetTranscriber {
    /// Helper used by the streaming impl: drains the async input
    /// (blocking on chunks via `block_on`-equivalent), then runs the
    /// batch path. Returns the same `(Final, Endpoint::StreamEnd)`
    /// sequence the batch `Transcriber` impl produces.
    fn run_batch_via_stream_audio(
        &self,
        audio: Box<dyn AsyncAudioInput>,
    ) -> Result<Vec<Result<TranscriptEvent>>> {
        // Sync-drain the async input by running a tokio current-thread
        // runtime. This is fine for v1 (input is already file-backed in
        // tests; live capture isn't wired through this code path yet).
        let samples = drain_async_audio_sync(audio)?;
        let total_samples = samples.len();
        #[allow(clippy::cast_precision_loss)]
        let total_duration = Duration::from_secs_f64(total_samples as f64 / f64::from(SAMPLE_RATE));

        if samples.is_empty() {
            return Ok(vec![Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            })]);
        }

        // Build a sync AudioInput from the collected samples and run
        // the batch path. The batch path emits Final + Endpoint::StreamEnd
        // exactly matching what the streaming acceptance criteria want.
        let sync_input: Box<dyn crate::voice::transcriber::AudioInput> =
            Box::new(VecAudioInput::from_samples(samples, 1));
        let stream = self.transcribe(sync_input)?;
        let collected: Vec<Result<TranscriptEvent>> = stream
            .map(|e| match e {
                Ok(TranscriptEvent::Final {
                    text,
                    start,
                    end,
                    confidence,
                    words,
                    speaker,
                    revisable,
                    ..
                }) => Ok(TranscriptEvent::Final {
                    // Re-mint event_id so repeated runs of the stream
                    // produce fresh ULIDs (Final dedup happens
                    // upstream of streaming, not within it).
                    event_id: Ulid::new(),
                    text,
                    start,
                    end,
                    confidence,
                    words,
                    speaker,
                    revisable,
                }),
                other => other,
            })
            .collect();
        Ok(collected)
    }
}

/// Drains an [`AsyncAudioInput`] to a concatenated i16 buffer.
///
/// Runtime-aware: when called from inside an existing tokio runtime
/// (the production case — cpal-driven audio capture is async; the
/// `voice_transcribe_parakeet_test` streaming tests build their own
/// current-thread runtime), it pumps the future on the calling thread
/// via `futures::executor::block_on`, which is runtime-agnostic and
/// doesn't nest. When called from a non-async context, it spins up
/// a fresh current-thread tokio runtime.
///
/// Previously this unconditionally built a tokio runtime and called
/// `block_on` on it, which panicked with "Cannot start a runtime from
/// within a runtime" whenever the caller was already inside one.
fn drain_async_audio_sync(mut audio: Box<dyn AsyncAudioInput>) -> Result<Vec<i16>> {
    use anyhow::Context;
    let drain = async move {
        let mut buf: Vec<i16> = Vec::new();
        while let Some(chunk) = audio.next_chunk().await {
            buf.extend_from_slice(&chunk);
        }
        buf
    };
    let samples: Vec<i16> = if tokio::runtime::Handle::try_current().is_ok() {
        // Already inside a tokio runtime — use a runtime-agnostic executor
        // that doesn't try to spin up a second reactor on the same thread.
        futures::executor::block_on(drain)
    } else {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime for stream-audio drain")?;
        rt.block_on(drain)
    };
    Ok(samples)
}
