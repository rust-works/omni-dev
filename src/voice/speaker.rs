//! Speaker embedding via `tract-onnx` + wespeaker, plus the persisted
//! enrolled-speaker JSON shape.
//!
//! The runtime choice (wespeaker `voxceleb_resnet34_LM` under
//! `tract-onnx`) is fixed by [ADR-0034](../../docs/adrs/adr-0034.md).
//! This module is the load-and-embed half; CLI plumbing for
//! `voice enroll` and `voice transcribe --speaker` lives in
//! [`crate::cli::voice`].

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};
use tract_onnx::prelude::*;

use crate::voice::features::{
    build_mel_filterbank, compute_fbank, FFT_SIZE, NUM_MEL_BINS, SAMPLE_RATE,
};

/// Numerical floor for L2 norm — prevents divide-by-zero when an
/// embedding is exactly the zero vector (vanishingly unlikely with
/// real audio, but cheap to guard against).
const L2_EPSILON: f32 = 1e-12;

/// Minimum PCM length (in 16 kHz samples) the embedder accepts. Below
/// this, the FBANK CMN is unstable and the embedding is meaningless.
pub const MIN_EMBED_SAMPLES: usize = SAMPLE_RATE as usize / 2; // 0.5 s

/// Type alias for the runnable tract-onnx plan shape that
/// [`WespeakerEmbedder`] owns. Spelled out once here so the struct field
/// stays readable.
type OnnxPlan = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

/// In-memory `tract-onnx` model + precomputed mel filterbank + FFT plan.
///
/// `Send + Sync` because [`SimplePlan::run`] takes `&self` and the
/// `Arc<dyn Fft<f32>>` from `rustfft` is also thread-safe.
pub struct WespeakerEmbedder {
    plan: OnnxPlan,
    mel_filters: Vec<Vec<f32>>,
    #[allow(dead_code)]
    fft: Arc<dyn Fft<f32>>,
}

impl WespeakerEmbedder {
    /// Loads the wespeaker ONNX graph at `model_path` and builds the
    /// FBANK mel filterbank + FFT plan that [`Self::embed`] reuses
    /// across calls.
    pub fn new(model_path: &Path) -> Result<Self> {
        if !model_path.is_file() {
            return Err(anyhow!(
                "wespeaker ONNX not found at {}; run `omni-dev voice install-model \
                 --variant speaker-wespeaker-en` or pass --speaker-model <path>",
                model_path.display()
            ));
        }
        let plan = tract_onnx::onnx()
            .model_for_path(model_path)
            .with_context(|| format!("load wespeaker ONNX at {}", model_path.display()))?
            .into_optimized()
            .context("optimize wespeaker ONNX")?
            .into_runnable()
            .context("make wespeaker ONNX runnable")?;
        let mel_filters = build_mel_filterbank(NUM_MEL_BINS, FFT_SIZE, SAMPLE_RATE)
            .context("build wespeaker mel filterbank")?;
        let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        Ok(Self {
            plan,
            mel_filters,
            fft,
        })
    }

    /// Embeds a 16 kHz mono `i16` PCM window into a 256-dim
    /// L2-normalised vector. Refuses windows shorter than
    /// [`MIN_EMBED_SAMPLES`].
    pub fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>> {
        if pcm.len() < MIN_EMBED_SAMPLES {
            bail!(
                "PCM window has {} samples; need at least {} (~0.5 s at 16 kHz) \
                 for a stable speaker embedding",
                pcm.len(),
                MIN_EMBED_SAMPLES
            );
        }
        let pcm_f32: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let features = compute_fbank(&pcm_f32, &self.mel_filters)?;
        let num_frames = features.len();

        let mut flat = Vec::with_capacity(num_frames * NUM_MEL_BINS);
        for frame in &features {
            flat.extend_from_slice(frame);
        }
        let tensor: Tensor =
            tract_ndarray::Array3::from_shape_vec((1, num_frames, NUM_MEL_BINS), flat)
                .context("build wespeaker feature tensor")?
                .into();
        let outputs = self
            .plan
            .run(tvec!(tensor.into()))
            .context("run wespeaker inference")?;
        let emb: Vec<f32> = outputs[0]
            .to_array_view::<f32>()
            .context("wespeaker output to f32 view")?
            .iter()
            .copied()
            .collect();
        Ok(l2_normalise(emb))
    }
}

/// L2-normalises the supplied vector in place semantically (returns a
/// new `Vec<f32>` with the same length).
pub fn l2_normalise(v: Vec<f32>) -> Vec<f32> {
    let norm = (v.iter().map(|x| x * x).sum::<f32>())
        .sqrt()
        .max(L2_EPSILON);
    v.into_iter().map(|x| x / norm).collect()
}

/// Cosine similarity between two equal-length vectors.
///
/// Returns the raw dot product (which equals cosine when both inputs
/// are L2-normalised). Panics if the lengths differ — only correct
/// inputs are passed in production code, and this is a small-arity
/// hot path.
#[allow(clippy::missing_panics_doc)]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "cosine: length mismatch");
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ── Enrolled speaker persistence ──────────────────────────────────────────

/// Persisted enrolment record. Lives at
/// `~/.omni-dev/voice/speakers/<name>.json`.
///
/// Matches the JSON shape from the original #805 spec; embedding files
/// are forward-compatible with extra fields via `serde`'s default
/// behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolledSpeaker {
    /// Speaker name — also the JSON filename stem.
    pub name: String,
    /// Identifier of the model that produced this embedding, matching
    /// the `variant` field of the model's `ModelSpec` (e.g.
    /// `"speaker-wespeaker-en"`).
    pub model: String,
    /// Embedding dimensionality — wespeaker resnet34_LM emits 256.
    pub dim: usize,
    /// L2-normalised embedding vector.
    pub vector: Vec<f32>,
    /// How many distinct PCM samples were used to build this embedding.
    /// v1 always stores `1` (single-sample enrolment); kept on disk for
    /// future multi-sample averaging.
    pub samples_used: u32,
    /// UTC timestamp of enrolment.
    pub enrolled_at: DateTime<Utc>,
}

impl EnrolledSpeaker {
    /// Loads a JSON enrolment file from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read enrolled-speaker JSON at {}", path.display()))?;
        let speaker: Self = serde_json::from_str(&body)
            .with_context(|| format!("parse enrolled-speaker JSON at {}", path.display()))?;
        if speaker.dim != speaker.vector.len() {
            bail!(
                "enrolled-speaker {} declares dim={} but vector has {} elements",
                path.display(),
                speaker.dim,
                speaker.vector.len()
            );
        }
        Ok(speaker)
    }

    /// Writes the enrolment to `path`, creating the parent directory if
    /// needed. Atomic via a `.tmp` sibling + rename so a crashed write
    /// can never leave a half-written JSON.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serialise enrolled-speaker JSON")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .with_context(|| format!("write enrolled-speaker JSON to {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn stub_speaker(name: &str) -> EnrolledSpeaker {
        EnrolledSpeaker {
            name: name.to_string(),
            model: "speaker-wespeaker-en".to_string(),
            dim: 4,
            vector: vec![0.5, 0.5, 0.5, 0.5],
            samples_used: 1,
            enrolled_at: Utc::now(),
        }
    }

    #[test]
    fn enrolled_speaker_save_load_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("alice.json");
        let original = stub_speaker("alice");
        original.save(&path).unwrap();
        let loaded = EnrolledSpeaker::load(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn enrolled_speaker_save_creates_parent_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nested/under/here/alice.json");
        stub_speaker("alice").save(&path).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn enrolled_speaker_save_is_atomic_no_tmp_leftover() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("alice.json");
        stub_speaker("alice").save(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "save left .tmp files behind");
    }

    #[test]
    fn enrolled_speaker_load_rejects_dim_vector_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("bad.json");
        let bad = serde_json::json!({
            "name": "bob",
            "model": "speaker-wespeaker-en",
            "dim": 256,
            "vector": [0.1, 0.2],
            "samples_used": 1,
            "enrolled_at": Utc::now().to_rfc3339()
        });
        std::fs::write(&path, bad.to_string()).unwrap();
        let err = EnrolledSpeaker::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("declares dim=256"), "got: {msg}");
        assert!(msg.contains("vector has 2 elements"), "got: {msg}");
    }

    #[test]
    fn enrolled_speaker_load_errors_on_missing_file() {
        let err = EnrolledSpeaker::load(Path::new("/nonexistent/x.json")).unwrap_err();
        assert!(err.to_string().contains("read enrolled-speaker JSON"));
    }

    #[test]
    fn enrolled_speaker_load_errors_on_malformed_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("bad.json");
        std::fs::write(&path, b"{not json").unwrap();
        let err = EnrolledSpeaker::load(&path).unwrap_err();
        assert!(err.to_string().contains("parse enrolled-speaker JSON"));
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = [1.0, 0.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0, 0.0];
        assert!((cosine(&a, &b) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_identical_l2_normed_is_one() {
        let a = l2_normalise(vec![1.0, 2.0, 3.0, 4.0]);
        let s = cosine(&a, &a);
        assert!((s - 1.0).abs() < 1e-6, "got: {s}");
    }

    #[test]
    fn l2_normalise_unit_length() {
        let v = l2_normalise(vec![3.0, 4.0]);
        let norm = v[0].hypot(v[1]);
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_normalise_handles_zero_vector() {
        // 0/eps is finite; the result should be the zero vector itself,
        // not NaN/Inf. cosine(zero, zero) = 0 which is the sensible
        // dot-product answer.
        let v = l2_normalise(vec![0.0, 0.0, 0.0]);
        assert!(v.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn wespeaker_embedder_new_errors_on_missing_file() {
        let Err(err) = WespeakerEmbedder::new(Path::new("/nope/wespeaker.onnx")) else {
            panic!("missing model file should error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("wespeaker ONNX not found"), "got: {msg}");
        assert!(msg.contains("--variant speaker-wespeaker-en"), "got: {msg}");
    }
}
