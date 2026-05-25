//! Spike #813: pure-Rust ASR runtime evaluation via `candle`.
//!
//! Loads `openai/whisper-tiny.en` from HF Hub, transcribes the WAV at
//! argv[1], prints the transcript to stdout, and emits the
//! inference-only latency to stderr. Greedy decode (temperature 0),
//! English-only, no timestamps. Cribbed from
//! `candle/candle-examples/examples/whisper` and slimmed to the smallest
//! surface that produces text.

use anyhow::{anyhow, bail, Context, Result};
use byteorder::{ByteOrder, LittleEndian};
use candle_core::{Device, IndexOp, Tensor};
use candle_nn::{ops::softmax, VarBuilder};
use candle_transformers::models::whisper::{self as m, audio, Config};
use hf_hub::{api::sync::Api, Repo, RepoType};
use std::path::PathBuf;
use std::time::Instant;
use tokenizers::Tokenizer;

const MODEL_ID: &str = "openai/whisper-tiny.en";
const REVISION: &str = "refs/pr/15";

fn main() -> Result<()> {
    let wav_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: spike-candle <path/to/16k-mono-i16.wav>"))?;

    let t_fetch = Instant::now();
    let (config_path, tokenizer_path, weights_path) = fetch_model()?;
    eprintln!("fetch_ms={}", t_fetch.elapsed().as_millis());

    let t_load = Instant::now();
    let device = Device::Cpu;
    let config: Config = serde_json::from_str(&std::fs::read_to_string(&config_path)?)
        .context("parse config.json")?;
    let tokenizer =
        Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow!("load tokenizer.json: {e}"))?;
    let vb = unsafe {
        VarBuilder::from_mmaped_safetensors(&[&weights_path], m::DTYPE, &device)
            .context("mmap safetensors weights")?
    };
    let model = m::model::Whisper::load(&vb, config.clone())?;
    eprintln!("model_load_ms={}", t_load.elapsed().as_millis());

    let mel_filters = load_mel_filters(config.num_mel_bins)?;
    let pcm = load_wav_as_f32(&wav_path)?;

    let t_infer = Instant::now();
    let mel = audio::pcm_to_mel(&config, &pcm, &mel_filters);
    let mel_len = mel.len();
    let mel = Tensor::from_vec(
        mel,
        (1, config.num_mel_bins, mel_len / config.num_mel_bins),
        &device,
    )?;
    let transcript = run_inference(model, &config, &tokenizer, &mel, &device)?;
    eprintln!("inference_ms={}", t_infer.elapsed().as_millis());

    println!("{}", transcript.trim());
    Ok(())
}

fn fetch_model() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let api = Api::new().context("hf-hub Api::new")?;
    let repo = api.repo(Repo::with_revision(
        MODEL_ID.to_string(),
        RepoType::Model,
        REVISION.to_string(),
    ));
    let config = repo.get("config.json").context("fetch config.json")?;
    let tokenizer = repo.get("tokenizer.json").context("fetch tokenizer.json")?;
    let weights = repo
        .get("model.safetensors")
        .context("fetch model.safetensors")?;
    Ok((config, tokenizer, weights))
}

fn load_mel_filters(num_mel_bins: usize) -> Result<Vec<f32>> {
    if num_mel_bins != 80 {
        bail!("spike only ships 80-bin mel filters (got {num_mel_bins})");
    }
    let bytes = include_bytes!("melfilters.bytes");
    let mut filters = vec![0f32; bytes.len() / 4];
    LittleEndian::read_f32_into(bytes, &mut filters);
    Ok(filters)
}

fn load_wav_as_f32(path: &str) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("open WAV at {path}"))?;
    let spec = reader.spec();
    if spec.sample_rate != m::SAMPLE_RATE as u32 {
        bail!(
            "WAV must be {} Hz (got {})",
            m::SAMPLE_RATE,
            spec.sample_rate
        );
    }
    if spec.channels != 1 {
        bail!("WAV must be mono (got {} channels)", spec.channels);
    }
    if spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
        bail!(
            "WAV must be 16-bit signed PCM (got {}-bit {:?})",
            spec.bits_per_sample,
            spec.sample_format
        );
    }
    let samples: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32768.0))
        .collect::<Result<Vec<_>, _>>()
        .context("decode i16 PCM")?;
    Ok(samples)
}

fn run_inference(
    mut model: m::model::Whisper,
    config: &Config,
    tokenizer: &Tokenizer,
    mel: &Tensor,
    device: &Device,
) -> Result<String> {
    let sot = token_id(tokenizer, m::SOT_TOKEN)?;
    let eot = token_id(tokenizer, m::EOT_TOKEN)?;
    let transcribe = token_id(tokenizer, m::TRANSCRIBE_TOKEN)?;
    let no_timestamps = token_id(tokenizer, m::NO_TIMESTAMPS_TOKEN)?;

    let suppress: Vec<f32> = (0..config.vocab_size as u32)
        .map(|i| {
            if config.suppress_tokens.contains(&i) {
                f32::NEG_INFINITY
            } else {
                0f32
            }
        })
        .collect();
    let suppress = Tensor::new(suppress.as_slice(), device)?;

    let (_, _, content_frames) = mel.dims3()?;
    let mut all_tokens: Vec<u32> = Vec::new();
    let mut seek = 0;

    while seek < content_frames {
        let segment_size = usize::min(content_frames - seek, m::N_FRAMES);
        let mel_segment = mel.narrow(2, seek, segment_size)?;
        seek += segment_size;

        let audio_features = model.encoder.forward(&mel_segment, true)?;
        let mut tokens: Vec<u32> = vec![sot, transcribe, no_timestamps];
        let sample_len = config.max_target_positions / 2;

        for i in 0..sample_len {
            let tokens_t = Tensor::new(tokens.as_slice(), device)?.unsqueeze(0)?;
            let ys = model.decoder.forward(&tokens_t, &audio_features, i == 0)?;
            let (_, seq_len, _) = ys.dims3()?;
            let logits = model
                .decoder
                .final_linear(&ys.i((..1, seq_len - 1..))?)?
                .i(0)?
                .i(0)?;
            let logits = logits.broadcast_add(&suppress)?;
            let probs = softmax(&logits, candle_core::D::Minus1)?;
            let probs_v: Vec<f32> = probs.to_vec1()?;
            let next = probs_v
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(i, _)| i as u32)
                .unwrap();
            if next == eot {
                break;
            }
            tokens.push(next);
            if tokens.len() > config.max_target_positions {
                break;
            }
        }

        for &t in &tokens[3..] {
            all_tokens.push(t);
        }
    }

    tokenizer
        .decode(&all_tokens, true)
        .map_err(|e| anyhow!("tokenizer.decode: {e}"))
}

fn token_id(tokenizer: &Tokenizer, token: &str) -> Result<u32> {
    tokenizer
        .token_to_id(token)
        .ok_or_else(|| anyhow!("no token-id for {token}"))
}
