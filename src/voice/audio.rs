//! Audio source abstraction.
//!
//! The [`AudioSource`] trait is the seam between hardware capture (real cpal
//! callbacks) and the rest of the pipeline. Production code uses
//! [`CpalAudioSource`] (step 7); tests drive the same pipeline through
//! [`FileAudioSource`], which replays samples from a fixture WAV.
//!
//! See ADR-0031 for the rationale behind keeping this seam at the f32-frame
//! level (rather than mocking cpal directly or asserting only at the CLI
//! level).

use std::path::Path;

use anyhow::{Context, Result};

/// Source of raw interleaved f32 audio samples at a fixed sample rate and
/// channel count.
///
/// Each call to [`AudioSource::next_chunk`] returns a freshly-allocated
/// `Vec<f32>` of interleaved frames (i.e. for stereo, samples alternate
/// L/R/L/R/…). `None` signals end-of-stream — the source is exhausted
/// (file end, cpal stream stopped, …) and will not produce more samples.
///
/// Implementations must be `Send` so the pipeline can drive them from a
/// background thread.
pub trait AudioSource: Send {
    /// Returns the next chunk of interleaved samples, or `None` when the
    /// source is exhausted.
    fn next_chunk(&mut self) -> Option<Vec<f32>>;
    /// The source's sample rate in Hz.
    fn sample_rate(&self) -> u32;
    /// Channel count (1 = mono, 2 = stereo, …).
    fn channels(&self) -> u16;
}

/// Test [`AudioSource`] that replays a fixture WAV in fixed-size chunks.
///
/// Samples are converted to f32 in `[-1.0, 1.0]` regardless of the fixture's
/// bit depth, so a single fixture can stand in for any capture-side input
/// rate the pipeline needs to exercise.
pub struct FileAudioSource {
    samples: Vec<f32>,
    cursor: usize,
    chunk_frames: usize,
    sample_rate: u32,
    channels: u16,
}

impl FileAudioSource {
    /// Loads a WAV file and prepares it for chunked playback.
    ///
    /// `chunk_frames` is the number of *frames* (not samples) returned per
    /// [`AudioSource::next_chunk`] call — i.e. for stereo at
    /// `chunk_frames = 1024`, each chunk contains 2048 interleaved samples.
    pub fn from_path(path: impl AsRef<Path>, chunk_frames: usize) -> Result<Self> {
        let path = path.as_ref();
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("Failed to open fixture WAV at {}", path.display()))?;
        let spec = reader.spec();
        let samples = read_all_samples_as_f32(&mut reader, spec)
            .with_context(|| format!("Failed to read samples from {}", path.display()))?;
        Ok(Self {
            samples,
            cursor: 0,
            chunk_frames: chunk_frames.max(1),
            sample_rate: spec.sample_rate,
            channels: spec.channels,
        })
    }

    /// Builds a fixture source directly from an in-memory sample buffer.
    /// Useful for synthesising test signals (sine waves, silence, …) without
    /// hitting disk.
    pub fn from_samples(
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
        chunk_frames: usize,
    ) -> Self {
        Self {
            samples,
            cursor: 0,
            chunk_frames: chunk_frames.max(1),
            sample_rate,
            channels,
        }
    }
}

impl AudioSource for FileAudioSource {
    fn next_chunk(&mut self) -> Option<Vec<f32>> {
        if self.cursor >= self.samples.len() {
            return None;
        }
        let samples_per_chunk = self.chunk_frames * self.channels as usize;
        let end = (self.cursor + samples_per_chunk).min(self.samples.len());
        let chunk = self.samples[self.cursor..end].to_vec();
        self.cursor = end;
        Some(chunk)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

fn read_all_samples_as_f32<R: std::io::Read>(
    reader: &mut hound::WavReader<R>,
    spec: hound::WavSpec,
) -> Result<Vec<f32>> {
    match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to decode f32 PCM samples"),
        hound::SampleFormat::Int => {
            let scale = i32_pcm_scale(spec.bits_per_sample);
            reader
                .samples::<i32>()
                .map(|res| res.map(|s| s as f32 / scale))
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to decode integer PCM samples")
        }
    }
}

fn i32_pcm_scale(bits_per_sample: u16) -> f32 {
    // `hound` decodes integer PCM as sign-extended i32 regardless of the
    // declared bit depth, so the divisor is always `2^(bits-1)`.
    let shift = bits_per_sample.saturating_sub(1);
    (1u64 << shift) as f32
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use anyhow::Result;
    use tempfile::TempDir;

    fn write_fixture_wav(
        dir: &TempDir,
        name: &str,
        sample_rate: u32,
        channels: u16,
        bits: u16,
        samples_i16: &[i16],
    ) -> Result<std::path::PathBuf> {
        let path = dir.path().join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec)?;
        for s in samples_i16 {
            writer.write_sample(*s)?;
        }
        writer.finalize()?;
        Ok(path)
    }

    #[test]
    fn file_source_returns_samples_in_chunks() -> Result<()> {
        let tmp = TempDir::new()?;
        // 12 mono i16 samples; 5 frames per chunk → 5, 5, 2.
        let path = write_fixture_wav(
            &tmp,
            "mono.wav",
            16_000,
            1,
            16,
            &[
                100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100, 1200,
            ],
        )?;
        let mut src = FileAudioSource::from_path(&path, 5)?;
        assert_eq!(src.sample_rate(), 16_000);
        assert_eq!(src.channels(), 1);
        let c1 = src.next_chunk().expect("first chunk");
        let c2 = src.next_chunk().expect("second chunk");
        let c3 = src.next_chunk().expect("third chunk");
        assert_eq!(c1.len(), 5);
        assert_eq!(c2.len(), 5);
        assert_eq!(c3.len(), 2);
        assert!(src.next_chunk().is_none());
        Ok(())
    }

    #[test]
    fn file_source_chunk_size_is_frames_not_samples_for_stereo() -> Result<()> {
        let tmp = TempDir::new()?;
        // 4 frames * 2 channels = 8 interleaved samples; chunk_frames = 2.
        let path = write_fixture_wav(&tmp, "stereo.wav", 48_000, 2, 16, &[1, 2, 3, 4, 5, 6, 7, 8])?;
        let mut src = FileAudioSource::from_path(&path, 2)?;
        assert_eq!(src.channels(), 2);
        let c1 = src.next_chunk().expect("chunk");
        assert_eq!(c1.len(), 4, "2 frames * 2 channels = 4 samples");
        let c2 = src.next_chunk().expect("chunk");
        assert_eq!(c2.len(), 4);
        assert!(src.next_chunk().is_none());
        Ok(())
    }

    #[test]
    fn file_source_decodes_i16_to_unit_range() -> Result<()> {
        let tmp = TempDir::new()?;
        let path = write_fixture_wav(&tmp, "edges.wav", 8000, 1, 16, &[i16::MAX, 0, i16::MIN])?;
        let mut src = FileAudioSource::from_path(&path, 16)?;
        let chunk = src.next_chunk().expect("chunk");
        // i16::MAX (32767) / 32768.0 ≈ 0.99997
        assert!((chunk[0] - 0.999_969_5).abs() < 1e-4);
        assert!((chunk[1] - 0.0).abs() < 1e-6);
        // i16::MIN (-32768) / 32768.0 = -1.0
        assert!((chunk[2] + 1.0).abs() < 1e-6);
        Ok(())
    }

    #[test]
    fn from_samples_round_trips_without_disk() {
        let samples = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut src = FileAudioSource::from_samples(samples.clone(), 16_000, 1, 4);
        let c1 = src.next_chunk().expect("first chunk");
        let c2 = src.next_chunk().expect("second chunk");
        assert_eq!(c1, samples[..4]);
        assert_eq!(c2, samples[4..]);
        assert!(src.next_chunk().is_none());
    }

    #[test]
    fn from_samples_yields_none_when_exhausted() {
        let mut src = FileAudioSource::from_samples(vec![0.0; 0], 16_000, 1, 32);
        assert!(src.next_chunk().is_none());
    }

    #[test]
    fn zero_chunk_size_is_treated_as_one_frame() {
        let mut src = FileAudioSource::from_samples(vec![0.1, 0.2, 0.3], 16_000, 1, 0);
        // chunk_frames clamped to 1 — one sample per chunk.
        let c1 = src.next_chunk().expect("c1");
        assert_eq!(c1, vec![0.1]);
        assert_eq!(src.next_chunk(), Some(vec![0.2]));
        assert_eq!(src.next_chunk(), Some(vec![0.3]));
        assert!(src.next_chunk().is_none());
    }
}
