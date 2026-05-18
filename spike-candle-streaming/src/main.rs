//! Spike: `candle` + LocalAgreement-2 sliding-window merger.
//!
//! Loads `whisper-tiny.en` (the same model `omni-dev voice install-model`
//! stages), reads a 16 kHz mono i16 WAV fixture, paces 100 ms chunks at
//! 100 ms wall-clock intervals, runs whisper on the accumulated window
//! every ~1 s of new audio, merges consecutive hypotheses via
//! LocalAgreement-2, and emits Partial / Final / SilenceOnset / Endpoint
//! events as JSONL.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use baseline::events::{EndpointKind, Event};
use baseline::idle::{IdleDetector, SAMPLE_RATE};
use byteorder::{ByteOrder, LittleEndian};
use candle_core::{Device, IndexOp, Tensor};
use candle_nn::{ops::softmax, VarBuilder};
use candle_transformers::models::whisper::{self as m, audio, Config};
use clap::Parser;
use tokenizers::Tokenizer;
use ulid::Ulid;

const MEL_FILTERS_80: &[u8] = include_bytes!("../../src/voice/backends/candle_melfilters.bytes");
const LOG_PROB_FLOOR: f32 = 1e-20;

/// Maximum audio window before a forced flush (matches whisper's natural
/// 30 s context limit; anything longer would degrade quality).
const MAX_WINDOW_SECS: f32 = 30.0;
/// Minimum window before kicking off inference at all.
const MIN_WINDOW_SECS: f32 = 2.0;

#[derive(Parser, Debug)]
struct Args {
    /// 16 kHz mono i16 PCM WAV.
    #[arg(long)]
    fixture: PathBuf,
    /// Directory containing `config.json`, `tokenizer.json`,
    /// `model.safetensors` (e.g. `~/.omni-dev/voice/models/whisper-tiny.en`).
    #[arg(long)]
    model_dir: PathBuf,
    /// Output JSONL event log.
    #[arg(long)]
    log: PathBuf,
    /// Seconds of consecutive silence before firing an Endpoint.
    #[arg(long, default_value_t = 2)]
    silence_secs: u32,
    /// Seconds of new audio between re-inferences (whisper_streaming's
    /// `min_chunk_size`, default 1.0).
    #[arg(long, default_value_t = 1.0)]
    min_chunk_secs: f32,
    /// Emit `Partial` events in addition to `Final`.
    #[arg(long, default_value_t = false)]
    emit_partials: bool,
    /// Disable the realtime pacing sleep — runs as fast as inference allows.
    /// Used to measure raw RTF independent of wall-clock pacing.
    #[arg(long, default_value_t = false)]
    no_pacing: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let process_start = Instant::now();
    let mut log = std::fs::File::create(&args.log)
        .with_context(|| format!("create log at {}", args.log.display()))?;

    let pcm = load_wav(&args.fixture)?;
    let total_audio_ms = (pcm.len() as u128 * 1000) / SAMPLE_RATE as u128;

    let load_start = Instant::now();
    let mut model = WhisperModel::load(&args.model_dir)?;
    let load_ms = load_start.elapsed().as_millis();
    write_event(
        &mut log,
        &Event::ModelLoaded {
            wall_ms: process_start.elapsed().as_millis(),
            load_ms,
        },
    )?;

    let chunk_samples: usize = SAMPLE_RATE / 10; // 100 ms = 1600 samples
    let chunk_duration = Duration::from_millis(100);

    let mut audio_window: VecDeque<f32> = VecDeque::new();
    let mut idle = IdleDetector::new(args.silence_secs);
    let mut hyp_prev_words: Vec<String> = Vec::new();
    let mut committed_word_count: usize = 0;
    let mut audio_ms_pushed: u128 = 0;
    let mut last_inference_audio_ms: u128 = 0;
    let mut was_idle = false;
    let mut total_inference_ms: u128 = 0;

    for chunk in pcm.chunks(chunk_samples) {
        if !args.no_pacing {
            std::thread::sleep(chunk_duration);
        }
        audio_ms_pushed += (chunk.len() as u128 * 1000) / SAMPLE_RATE as u128;
        audio_window.extend(chunk.iter().copied());
        idle.push(chunk);

        // Hard cap: window can't exceed 30 s. We don't try to slide
        // sub-utterance; we let the silence-gap trigger reset state.
        // If the cap fires *without* a silence gap, force a flush.
        let window_secs = audio_window.len() as f32 / SAMPLE_RATE as f32;
        if window_secs > MAX_WINDOW_SECS {
            flush_remaining(
                &mut log,
                &process_start,
                audio_ms_pushed,
                &hyp_prev_words,
                committed_word_count,
                EndpointKind::SilenceGap,
            )?;
            audio_window.clear();
            hyp_prev_words.clear();
            committed_word_count = 0;
            last_inference_audio_ms = audio_ms_pushed;
            idle = IdleDetector::new(args.silence_secs);
            was_idle = false;
            continue;
        }

        let now_idle = idle.is_idle();
        if !was_idle && now_idle {
            write_event(
                &mut log,
                &Event::SilenceOnset {
                    wall_ms: process_start.elapsed().as_millis(),
                    audio_ms: audio_ms_pushed,
                },
            )?;
            // Run a final inference pass on whatever's in the window,
            // then flush + endpoint, then reset.
            if audio_window.len() >= (MIN_WINDOW_SECS * SAMPLE_RATE as f32) as usize {
                let pcm_now: Vec<f32> = audio_window.iter().copied().collect();
                let infer_start = Instant::now();
                let hyp = model.transcribe(&pcm_now)?;
                total_inference_ms += infer_start.elapsed().as_millis();
                let words: Vec<String> = hyp.split_whitespace().map(String::from).collect();
                let lcp = longest_common_prefix(&hyp_prev_words, &words);
                if lcp > committed_word_count {
                    emit_final(
                        &mut log,
                        &process_start,
                        audio_ms_pushed,
                        &words[committed_word_count..lcp],
                    )?;
                }
                // Flush everything beyond LCP as a final commit on silence.
                let flush_start = lcp.max(committed_word_count);
                if flush_start < words.len() {
                    emit_final(
                        &mut log,
                        &process_start,
                        audio_ms_pushed,
                        &words[flush_start..],
                    )?;
                }
                hyp_prev_words = words;
            }
            write_event(
                &mut log,
                &Event::Endpoint {
                    wall_ms: process_start.elapsed().as_millis(),
                    audio_ms: audio_ms_pushed,
                    kind: EndpointKind::SilenceGap,
                },
            )?;
            audio_window.clear();
            hyp_prev_words.clear();
            committed_word_count = 0;
            last_inference_audio_ms = audio_ms_pushed;
            idle = IdleDetector::new(args.silence_secs);
            was_idle = false;
            continue;
        }
        was_idle = now_idle;

        let new_audio_since_inf = audio_ms_pushed - last_inference_audio_ms;
        let cadence_ms = (args.min_chunk_secs * 1000.0) as u128;
        if window_secs >= MIN_WINDOW_SECS && new_audio_since_inf >= cadence_ms {
            let pcm_now: Vec<f32> = audio_window.iter().copied().collect();
            let infer_start = Instant::now();
            let hyp = model.transcribe(&pcm_now)?;
            total_inference_ms += infer_start.elapsed().as_millis();
            let words: Vec<String> = hyp.split_whitespace().map(String::from).collect();

            let lcp = longest_common_prefix(&hyp_prev_words, &words);
            if lcp > committed_word_count {
                emit_final(
                    &mut log,
                    &process_start,
                    audio_ms_pushed,
                    &words[committed_word_count..lcp],
                )?;
                committed_word_count = lcp;
            }
            if args.emit_partials && words.len() > committed_word_count {
                let partial_text = words[committed_word_count..].join(" ");
                write_event(
                    &mut log,
                    &Event::Partial {
                        wall_ms: process_start.elapsed().as_millis(),
                        audio_ms: audio_ms_pushed,
                        text: partial_text,
                    },
                )?;
            }
            hyp_prev_words = words;
            last_inference_audio_ms = audio_ms_pushed;
        }
    }

    // End-of-stream: flush whatever's left.
    if audio_window.len() >= (MIN_WINDOW_SECS * SAMPLE_RATE as f32) as usize {
        let pcm_now: Vec<f32> = audio_window.iter().copied().collect();
        let infer_start = Instant::now();
        let hyp = model.transcribe(&pcm_now)?;
        total_inference_ms += infer_start.elapsed().as_millis();
        let words: Vec<String> = hyp.split_whitespace().map(String::from).collect();
        if words.len() > committed_word_count {
            emit_final(
                &mut log,
                &process_start,
                audio_ms_pushed,
                &words[committed_word_count..],
            )?;
        }
    }
    write_event(
        &mut log,
        &Event::Endpoint {
            wall_ms: process_start.elapsed().as_millis(),
            audio_ms: audio_ms_pushed,
            kind: EndpointKind::StreamEnd,
        },
    )?;
    write_event(
        &mut log,
        &Event::StreamEnd {
            wall_ms: process_start.elapsed().as_millis(),
            audio_ms: audio_ms_pushed,
        },
    )?;

    eprintln!(
        "spike-candle-streaming: total_audio_ms={total_audio_ms} \
         total_inference_ms={total_inference_ms} \
         rtf={:.3} model_load_ms={load_ms}",
        total_inference_ms as f64 / total_audio_ms as f64
    );
    Ok(())
}

fn flush_remaining(
    log: &mut std::fs::File,
    process_start: &Instant,
    audio_ms_pushed: u128,
    hyp_prev_words: &[String],
    committed_word_count: usize,
    kind: EndpointKind,
) -> Result<()> {
    if hyp_prev_words.len() > committed_word_count {
        emit_final(
            log,
            process_start,
            audio_ms_pushed,
            &hyp_prev_words[committed_word_count..],
        )?;
    }
    write_event(
        log,
        &Event::Endpoint {
            wall_ms: process_start.elapsed().as_millis(),
            audio_ms: audio_ms_pushed,
            kind,
        },
    )
}

fn emit_final(
    log: &mut std::fs::File,
    process_start: &Instant,
    audio_ms: u128,
    words: &[String],
) -> Result<()> {
    if words.is_empty() {
        return Ok(());
    }
    write_event(
        log,
        &Event::Final {
            wall_ms: process_start.elapsed().as_millis(),
            audio_ms,
            event_id: Ulid::new().to_string(),
            text: words.join(" "),
            confidence: 1.0,
        },
    )
}

fn write_event(log: &mut std::fs::File, event: &Event) -> Result<()> {
    use std::io::Write;
    let line = serde_json::to_string(event)?;
    writeln!(log, "{line}").context("write event")
}

fn longest_common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn load_wav(path: &PathBuf) -> Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("open WAV at {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != 16_000 || spec.channels != 1 || spec.bits_per_sample != 16 {
        bail!(
            "fixture must be 16 kHz mono 16-bit PCM; got {} Hz, {} ch, {} bps",
            spec.sample_rate,
            spec.channels,
            spec.bits_per_sample
        );
    }
    let samples: Result<Vec<i16>, _> = reader.samples::<i16>().collect();
    let samples = samples.context("decode WAV samples")?;
    Ok(samples
        .into_iter()
        .map(|s| f32::from(s) / 32768.0)
        .collect())
}

struct WhisperModel {
    model: m::model::Whisper,
    config: Config,
    tokenizer: Tokenizer,
    mel_filters: Vec<f32>,
    suppress: Tensor,
    device: Device,
    sot: u32,
    eot: u32,
    transcribe: u32,
    no_timestamps: u32,
}

impl WhisperModel {
    fn load(model_dir: &PathBuf) -> Result<Self> {
        let config_path = model_dir.join("config.json");
        let tokenizer_path = model_dir.join("tokenizer.json");
        let weights_path = model_dir.join("model.safetensors");
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path)
                .with_context(|| format!("read {}", config_path.display()))?,
        )
        .context("parse config")?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow!("load tokenizer at {}: {e}", tokenizer_path.display()))?;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&weights_path], m::DTYPE, &device)
                .with_context(|| format!("mmap weights at {}", weights_path.display()))?
        };
        let model = m::model::Whisper::load(&vb, config.clone()).context("load whisper")?;
        let mel_filters = load_mel_filters(config.num_mel_bins)?;
        let suppress = build_suppress_tensor(&config, &device)?;
        let sot = token_id(&tokenizer, m::SOT_TOKEN)?;
        let eot = token_id(&tokenizer, m::EOT_TOKEN)?;
        let transcribe = token_id(&tokenizer, m::TRANSCRIBE_TOKEN)?;
        let no_timestamps = token_id(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?;
        Ok(Self {
            model,
            config,
            tokenizer,
            mel_filters,
            suppress,
            device,
            sot,
            eot,
            transcribe,
            no_timestamps,
        })
    }

    fn transcribe(&mut self, pcm: &[f32]) -> Result<String> {
        let mel = audio::pcm_to_mel(&self.config, pcm, &self.mel_filters);
        let mel_len = mel.len();
        let mel = Tensor::from_vec(
            mel,
            (
                1,
                self.config.num_mel_bins,
                mel_len / self.config.num_mel_bins,
            ),
            &self.device,
        )?;
        let (_, _, content_frames) = mel.dims3()?;
        let mut full_text = String::new();
        let mut seek = 0usize;
        while seek < content_frames {
            let segment_size = usize::min(content_frames - seek, m::N_FRAMES);
            let mel_segment = mel.narrow(2, seek, segment_size)?;
            seek += segment_size;
            let audio_features = self.model.encoder.forward(&mel_segment, true)?;
            let mut tokens: Vec<u32> = vec![self.sot, self.transcribe, self.no_timestamps];
            let sample_len = self.config.max_target_positions / 2;
            for i in 0..sample_len {
                let tokens_t = Tensor::new(tokens.as_slice(), &self.device)?.unsqueeze(0)?;
                let ys = self
                    .model
                    .decoder
                    .forward(&tokens_t, &audio_features, i == 0)?;
                let (_, seq_len, _) = ys.dims3()?;
                let logits = self
                    .model
                    .decoder
                    .final_linear(&ys.i((..1, seq_len - 1..))?)?
                    .i(0)?
                    .i(0)?;
                let logits = logits.broadcast_add(&self.suppress)?;
                let probs = softmax(&logits, candle_core::D::Minus1)?;
                let probs_v: Vec<f32> = probs.to_vec1()?;
                let (next_idx, _) = probs_v
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.total_cmp(b))
                    .map(|(i, p)| (i as u32, *p))
                    .ok_or_else(|| anyhow!("empty probability distribution"))?;
                let _ = LOG_PROB_FLOOR; // silence dead-code lint in early returns
                if next_idx == self.eot {
                    break;
                }
                tokens.push(next_idx);
                if tokens.len() > self.config.max_target_positions {
                    break;
                }
            }
            let segment_tokens = &tokens[3..];
            let text = self
                .tokenizer
                .decode(segment_tokens, true)
                .map_err(|e| anyhow!("decode segment tokens: {e}"))?;
            if !full_text.is_empty() {
                full_text.push(' ');
            }
            full_text.push_str(&text);
        }
        Ok(full_text.trim().to_string())
    }
}

fn load_mel_filters(num_mel_bins: usize) -> Result<Vec<f32>> {
    if num_mel_bins != 80 {
        bail!("80-bin mel filters only (got {num_mel_bins})");
    }
    let mut filters = vec![0f32; MEL_FILTERS_80.len() / 4];
    LittleEndian::read_f32_into(MEL_FILTERS_80, &mut filters);
    Ok(filters)
}

fn build_suppress_tensor(config: &Config, device: &Device) -> Result<Tensor> {
    let mask: Vec<f32> = (0..config.vocab_size as u32)
        .map(|i| {
            if config.suppress_tokens.contains(&i) {
                f32::NEG_INFINITY
            } else {
                0f32
            }
        })
        .collect();
    Tensor::new(mask.as_slice(), device).context("build suppress tensor")
}

fn token_id(tokenizer: &Tokenizer, token: &str) -> Result<u32> {
    tokenizer
        .token_to_id(token)
        .ok_or_else(|| anyhow!("tokenizer missing token {token}"))
}
