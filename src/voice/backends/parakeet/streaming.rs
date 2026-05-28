//! Streaming Parakeet wrapper — `StreamingTranscriber` impl on
//! [`CandleParakeetTranscriber`].
//!
//! **v3 (this commit)**: async yield-as-you-go. The previous v2 impl built
//! the full `Vec<TranscriptEvent>` synchronously inside the stream closure
//! and yielded via `stream::iter`; consumers saw all events in a burst at
//! end-of-input. v3 drives the session from a `futures::stream::try_unfold`
//! state machine that awaits the next chunk, runs `add_audio` /
//! `finalize` on a `tokio::task::spawn_blocking` thread (the encoder /
//! decoder forwards are CPU-blocking), and yields each event as soon as
//! it's produced. Under a realtime input (`FileAsyncAudioInput` with
//! sleeps, or live cpal), Partial events arrive *during* the stream.
//!
//! Per-chunk algorithm is unchanged from v2 — incremental local-window
//! attention + per-layer [`RotatingConformerCache`], mirroring
//! `parakeet_mlx::parakeet::StreamingParakeet.add_audio` from
//! `newhoggy/parakeet-mlx@32b8034`.
//!
//! ## Notable remaining limitations (to address in follow-ups)
//!
//! - **No silence-gap endpoint detection**: still emits
//!   `Endpoint::StreamEnd` only. Silence-gap-driven `Endpoint::SilenceGap`
//!   needs `IdleDetector` integration; deferred.
//!
//! ## Algorithm (per add_audio)
//!
//! 1. Append PCM (i16 → f32 in [-1, 1]) to `audio_buffer`.
//! 2. Compute raw log-mel for `audio_buffer` via `ParakeetMel::streaming_chunk`
//!    (which Welford-updates `running_stats` from the un-normalised mel,
//!    then normalises against the *current* running mean/std).
//! 3. Append new mel frames to `mel_buffer`, trim `audio_buffer` by
//!    `new_frames * HOP_LENGTH` samples (the audio that produced those
//!    frames).
//! 4. Align `mel_buffer` to a multiple of `subsampling_factor` (8);
//!    feed the aligned prefix to `FastConformerEncoderLocal::forward_with_cache`.
//! 5. Trim `mel_buffer` to keep `drop_size * subsampling_factor + leftover`
//!    tail frames — the last `drop_size` encoder frames' input plus the
//!    leftover that didn't align. These get re-encoded next call.
//! 6. Finalized decode: `decode_greedy_stateful` on the first
//!    `length - drop_size` encoder frames with persistent `(last_token,
//!    decoder_state)`. Returned state persists.
//! 7. Draft decode: same on the last `drop_size` frames from the
//!    finalized post-state; returned tokens are stored in `draft_tokens`
//!    for display only, state discarded.
//! 8. Emit `Partial { text: tokenizer.decode(finalized + draft) }`.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use candle_core::{Device, Tensor};
use futures::stream::{self, Stream};
use ulid::Ulid;

use super::audio::{ParakeetMel, RunningStats, HOP_LENGTH, N_MELS, SAMPLE_RATE};
use super::cache::RotatingConformerCache;
use super::decoder::{LstmState, TdtDecoder};
use super::encoder::FastConformerEncoderLocal;
use super::tokenizer::ParakeetTokenizer;
use super::CandleParakeetTranscriber;
use crate::voice::transcriber::{
    AsyncAudioInput, EndpointKind, StreamingTranscriber, TranscriptEvent,
};

/// Default local-attention context window for streaming: `(left, right)`.
///
/// Left context (256 encoder frames ≈ 20 s of past) is large for
/// attention-history quality; it only sizes the KV cache, not the
/// per-chunk re-encode window. Right context (16 frames ≈ 1.3 s of
/// lookahead) is small to keep both streaming latency AND the re-encode
/// window small: the window the encoder re-processes each chunk is
/// `drop_size × subsampling_factor` mel frames, and
/// `drop_size = right_context × depth`. With `(256, 256) / depth=4` that
/// window is ~82 s of audio (RTF ~14, unusable); with `(256, 16) /
/// depth=1` it's ~1.3 s (RTF well under 1).
///
/// The MLX fork validated `(256, 256)` at depth ∈ {1, 4, 24}; this port
/// trades the larger right context for tractable CPU cost given the
/// non-Metal-kernel local attention. WER impact is bounded by the
/// streaming-vs-batch parity check in the tests.
const DEFAULT_CONTEXT_SIZE: (usize, usize) = (256, 16);

/// Default depth (per-layer KV-cache exact match across chunks).
const DEFAULT_DEPTH: usize = 1;

/// 8× temporal subsampling at the encoder front-end.
const SUBSAMPLING_FACTOR: usize = 8;

/// Internal chunk-merge threshold. Per-encoder-forward overhead is largely
/// independent of window size (the candle op count is ~constant in T,
/// only matmul WORK grows with T), so processing larger windows amortises
/// overhead. We buffer incoming source chunks until the accumulator
/// reaches this many samples (≈ 5 s @ 16 kHz), then process.
///
/// Trade-off: Partial events arrive at the merged-chunk cadence rather
/// than the source-chunk cadence. For the 30 s streaming test (source
/// chunks 1.6 s, internal min 5 s), this is ~6 Partials instead of 19.
const INTERNAL_CHUNK_MIN_SAMPLES: usize = 80_000;

impl StreamingTranscriber for CandleParakeetTranscriber {
    fn transcribe_stream(
        &self,
        audio: Box<dyn AsyncAudioInput>,
    ) -> Pin<Box<dyn Stream<Item = Result<TranscriptEvent>> + Send>> {
        // Build the local-attention encoder once. Arc-cloning all weight
        // tensors is cheap; the resulting struct is owned and can be moved
        // across `spawn_blocking` boundaries.
        let d_model = 1024_usize;
        let local_encoder = match FastConformerEncoderLocal::from_full(
            &self.encoder,
            DEFAULT_CONTEXT_SIZE,
            d_model,
            /* scale_input */ false,
            &self.device,
        )
        .context("build local encoder")
        {
            Ok(e) => Arc::new(e),
            Err(err) => return Box::pin(stream::once(async move { Err(err) })),
        };

        let drop_size = DEFAULT_CONTEXT_SIZE.1 * DEFAULT_DEPTH;
        let session = match StreamingSession::new(
            Arc::clone(&local_encoder),
            Arc::clone(&self.decoder),
            Arc::clone(&self.tokenizer),
            Arc::clone(&self.mel),
            self.device.clone(),
            local_encoder.n_layers(),
            drop_size,
        ) {
            Ok(s) => s,
            Err(err) => return Box::pin(stream::once(async move { Err(err) })),
        };

        let state = DriverState {
            audio,
            session: Some(session),
            accumulator: Vec::with_capacity(INTERNAL_CHUNK_MIN_SAMPLES),
            queued: VecDeque::new(),
            finalized: false,
        };

        Box::pin(stream::try_unfold(state, drive))
    }
}

/// State threaded through the `try_unfold` driver.
///
/// `session` is `Option<_>` so we can `take()` it across `spawn_blocking`
/// boundaries (which require `'static` closures) and put it back when
/// the blocking work returns.
struct DriverState {
    audio: Box<dyn AsyncAudioInput>,
    session: Option<StreamingSession>,
    accumulator: Vec<i16>,
    queued: VecDeque<TranscriptEvent>,
    finalized: bool,
}

async fn drive(mut s: DriverState) -> Result<Option<(TranscriptEvent, DriverState)>> {
    loop {
        // Drain any queued events from a prior `add_audio` / `finalize`.
        if let Some(ev) = s.queued.pop_front() {
            return Ok(Some((ev, s)));
        }
        if s.finalized {
            return Ok(None);
        }

        if let Some(chunk) = s.audio.next_chunk().await {
            let mut session = take_session(&mut s.session)?;
            session.total_audio_samples += chunk.len();
            s.accumulator.extend_from_slice(&chunk);
            if s.accumulator.len() < INTERNAL_CHUNK_MIN_SAMPLES {
                s.session = Some(session);
                continue;
            }
            let buf = std::mem::replace(
                &mut s.accumulator,
                Vec::with_capacity(INTERNAL_CHUNK_MIN_SAMPLES),
            );
            let (returned, events) = tokio::task::spawn_blocking(
                move || -> Result<(StreamingSession, Vec<TranscriptEvent>)> {
                    let evs = session.add_audio(&buf)?;
                    Ok((session, evs))
                },
            )
            .await
            .context("spawn_blocking add_audio join")??;
            s.session = Some(returned);
            s.queued.extend(events);
        } else {
            let session = take_session(&mut s.session)?;
            if session.total_audio_samples == 0 {
                // Empty input: emit only the StreamEnd endpoint, no Final.
                s.queued.push_back(TranscriptEvent::Endpoint {
                    at: Duration::ZERO,
                    kind: EndpointKind::StreamEnd,
                });
            } else {
                let residual = std::mem::take(&mut s.accumulator);
                let events =
                    tokio::task::spawn_blocking(move || -> Result<Vec<TranscriptEvent>> {
                        let mut session = session;
                        let mut evs: Vec<TranscriptEvent> = Vec::new();
                        if !residual.is_empty() {
                            evs.extend(session.add_audio(&residual)?);
                        }
                        evs.extend(session.finalize()?);
                        Ok(evs)
                    })
                    .await
                    .context("spawn_blocking finalize join")??;
                s.queued.extend(events);
            }
            s.finalized = true;
        }
    }
}

/// Removes the session from the driver state, returning a structured error
/// rather than panicking if it's missing. The driver always replaces
/// the session before the next loop iteration that might re-take it, so
/// the `None` branch is unreachable in practice — this just keeps the
/// invariant local rather than implicit in an `expect`.
fn take_session(slot: &mut Option<StreamingSession>) -> Result<StreamingSession> {
    slot.take()
        .ok_or_else(|| anyhow!("Parakeet streaming session missing — driver invariant broken"))
}

/// Per-session streaming state. Owns its model components (via [`Arc`]) so
/// the whole session can be moved across [`tokio::task::spawn_blocking`]
/// boundaries from the async driver — no lifetimes, no held mutex
/// guards across `.await` points.
struct StreamingSession {
    encoder: Arc<FastConformerEncoderLocal>,
    decoder: Arc<TdtDecoder>,
    tokenizer: Arc<ParakeetTokenizer>,
    mel: Arc<ParakeetMel>,
    device: Device,

    audio_buffer: Vec<f32>,
    mel_buffer: Option<Tensor>,
    running_stats: RunningStats,
    layer_cache: Vec<Option<RotatingConformerCache>>,
    decoder_state: LstmState,
    last_token: Option<u32>,
    finalized_tokens: Vec<u32>,
    draft_tokens: Vec<u32>,
    drop_size: usize,
    total_audio_samples: usize,
}

impl StreamingSession {
    fn new(
        encoder: Arc<FastConformerEncoderLocal>,
        decoder: Arc<TdtDecoder>,
        tokenizer: Arc<ParakeetTokenizer>,
        mel: Arc<ParakeetMel>,
        device: Device,
        n_layers: usize,
        drop_size: usize,
    ) -> Result<Self> {
        // Initialise each layer's cache with capacity = left_context and
        // cache_drop_size = drop_size (= right_context * depth).
        let mut layer_cache: Vec<Option<RotatingConformerCache>> = Vec::with_capacity(n_layers);
        for _ in 0..n_layers {
            layer_cache.push(Some(RotatingConformerCache::new(
                DEFAULT_CONTEXT_SIZE.0,
                drop_size,
            )));
        }

        let decoder_state = decoder
            .predictor()
            .zero_state(1, &device)
            .context("predictor zero_state for streaming session")?;

        Ok(Self {
            encoder,
            decoder,
            tokenizer,
            mel,
            device,
            audio_buffer: Vec::with_capacity(16_000),
            mel_buffer: None,
            running_stats: RunningStats::new(),
            layer_cache,
            decoder_state,
            last_token: None,
            finalized_tokens: Vec::new(),
            draft_tokens: Vec::new(),
            drop_size,
            total_audio_samples: 0,
        })
    }

    /// Process one chunk of i16 PCM and return any emitted events.
    fn add_audio(&mut self, chunk_i16: &[i16]) -> Result<Vec<TranscriptEvent>> {
        // 1. Append PCM as f32 in [-1, 1].
        self.audio_buffer.reserve(chunk_i16.len());
        for &s in chunk_i16 {
            self.audio_buffer.push(f32::from(s) / 32768.0);
        }

        // 2. Compute new mel frames over the entire audio_buffer (the
        //    mel front-end is stateless w.r.t. frames — each call
        //    produces (audio_len - WIN_LENGTH) / HOP_LENGTH + 1 frames
        //    if there are enough samples).
        let mel_frames = self
            .mel
            .streaming_chunk(&self.audio_buffer, &mut self.running_stats)
            .context("mel streaming_chunk")?;
        if mel_frames.n_frames == 0 {
            return Ok(Vec::new());
        }

        // 3. Trim audio_buffer by the audio that produced those frames
        //    (n_frames * HOP_LENGTH samples). What remains is the
        //    overlap region (WIN_LENGTH - HOP_LENGTH samples) plus any
        //    sub-HOP_LENGTH residual.
        let consumed = mel_frames.n_frames * HOP_LENGTH;
        self.audio_buffer.drain(..consumed);

        // 4. Append normalised mel frames to mel_buffer.
        let new_mel = Tensor::from_vec(
            mel_frames.data,
            (1, mel_frames.n_frames, N_MELS),
            &self.device,
        )
        .context("build new mel tensor")?;
        let mel_buffer = match self.mel_buffer.take() {
            Some(prev) => Tensor::cat(&[&prev, &new_mel], 1)
                .context("append new mel to mel_buffer")?
                .contiguous()
                .context("contiguous mel_buffer")?,
            None => new_mel,
        };

        // 5. Align mel_buffer to a multiple of SUBSAMPLING_FACTOR.
        let total_mel_frames = mel_buffer.dim(1).context("mel_buffer dim 1")?;
        let aligned = (total_mel_frames / SUBSAMPLING_FACTOR) * SUBSAMPLING_FACTOR;
        if aligned == 0 {
            self.mel_buffer = Some(mel_buffer);
            return Ok(Vec::new());
        }
        let leftover = total_mel_frames - aligned;
        let mel_aligned = mel_buffer
            .narrow(1, 0, aligned)?
            .contiguous()
            .context("contiguous mel_aligned")?;

        // 6. Run encoder with cache.
        let features = self
            .encoder
            .forward_with_cache(&mel_aligned, &mut self.layer_cache)
            .context("encoder forward_with_cache")?;
        let length = features.dim(1).context("encoder features dim 1")?;

        // 7. Trim mel_buffer to keep (drop_size * SUBSAMPLING_FACTOR + leftover)
        //    tail frames. The dropped prefix has been encoded (cached);
        //    the kept tail will be re-encoded next call.
        let tail_len = self.drop_size * SUBSAMPLING_FACTOR + leftover;
        let tail_len = tail_len.min(total_mel_frames);
        let mel_tail_start = total_mel_frames - tail_len;
        self.mel_buffer = if tail_len == 0 {
            None
        } else {
            Some(
                mel_buffer
                    .narrow(1, mel_tail_start, tail_len)?
                    .contiguous()
                    .context("contiguous mel tail")?,
            )
        };

        // 8. Finalized decode (length - drop_size frames) with persistent
        //    state. Draft decode (last drop_size frames) from the
        //    post-finalized state; output stored in draft_tokens, state
        //    discarded.
        let finalized_length = length.saturating_sub(self.drop_size);
        if finalized_length > 0 {
            let (new_tokens, new_last, new_state) = self
                .decoder
                .decode_greedy_stateful(
                    &features,
                    finalized_length,
                    self.last_token,
                    self.decoder_state.clone(),
                )
                .context("finalized decode_greedy_stateful")?;
            self.finalized_tokens.extend(new_tokens);
            self.last_token = new_last;
            self.decoder_state = new_state;
        }

        // Draft decode: from the JUST-updated finalized state, decode the
        // remaining draft region. Results stored but state discarded.
        self.draft_tokens.clear();
        if length > finalized_length {
            // Slice encoder features to just the draft region.
            let draft_features = features
                .narrow(1, finalized_length, length - finalized_length)?
                .contiguous()
                .context("contiguous draft features")?;
            let (draft, _new_last, _new_state) = self
                .decoder
                .decode_greedy_stateful(
                    &draft_features,
                    length - finalized_length,
                    self.last_token,
                    self.decoder_state.clone(),
                )
                .context("draft decode_greedy_stateful")?;
            self.draft_tokens = draft;
        }

        // 9. Emit Partial with finalized + draft text.
        let mut combined: Vec<u32> =
            Vec::with_capacity(self.finalized_tokens.len() + self.draft_tokens.len());
        combined.extend(self.finalized_tokens.iter().copied());
        combined.extend(self.draft_tokens.iter().copied());
        let text = self
            .tokenizer
            .decode(&combined)
            .context("tokenizer decode (Partial)")?;
        if text.is_empty() {
            return Ok(Vec::new());
        }

        let elapsed = self.elapsed();
        Ok(vec![TranscriptEvent::Partial {
            text,
            start: Duration::ZERO,
            end: elapsed,
            words: None,
            speaker: None,
        }])
    }

    /// Emit the final Final + Endpoint::StreamEnd for the accumulated
    /// finalized tokens. Called when the AsyncAudioInput is exhausted.
    fn finalize(&self) -> Result<Vec<TranscriptEvent>> {
        let elapsed = self.elapsed();
        let mut combined: Vec<u32> =
            Vec::with_capacity(self.finalized_tokens.len() + self.draft_tokens.len());
        combined.extend(self.finalized_tokens.iter().copied());
        // At stream end, also commit the draft tokens — there's no future
        // audio to revise them. Mirrors the MLX fork's behaviour of
        // returning finalized + draft from .result.
        combined.extend(self.draft_tokens.iter().copied());
        let text = self
            .tokenizer
            .decode(&combined)
            .context("tokenizer decode (Final)")?;

        Ok(vec![
            TranscriptEvent::Final {
                event_id: Ulid::new(),
                text,
                start: Duration::ZERO,
                end: elapsed,
                confidence: 1.0,
                words: None,
                speaker: None,
                // `revisable: true` matches #806's trait-level contract:
                // downstream consumers (`voice listen`, reflection
                // trigger) treat all streaming Finals uniformly and rely
                // on the subsequent `Endpoint` to know text is settled.
                revisable: true,
            },
            TranscriptEvent::Endpoint {
                at: elapsed,
                kind: EndpointKind::StreamEnd,
            },
        ])
    }

    fn elapsed(&self) -> Duration {
        #[allow(clippy::cast_precision_loss)]
        Duration::from_secs_f64(self.total_audio_samples as f64 / f64::from(SAMPLE_RATE))
    }
}
