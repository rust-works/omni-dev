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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapRb};

/// Source of raw interleaved f32 audio samples at a fixed sample rate and
/// channel count.
///
/// Each call to [`AudioSource::next_chunk`] returns a freshly-allocated
/// `Vec<f32>` of interleaved frames (i.e. for stereo, samples alternate
/// L/R/L/R/…). `None` signals end-of-stream — the source is exhausted
/// (file end, cpal stream stopped, …) and will not produce more samples.
///
/// The trait is intentionally not `Send`: on macOS, cpal's `Stream` is
/// not `Send` (it holds a CoreAudio `AudioUnit` containing raw pointers),
/// so requiring `Send` here would force `CpalAudioSource` into an
/// awkward indirection. The capture pipeline runs synchronously on the
/// owning thread — the cpal callback runs on cpal's own audio thread and
/// communicates through a lock-free SPSC ring buffer.
pub trait AudioSource {
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

/// Maximum samples per [`AudioSource::next_chunk`] call for `CpalAudioSource`.
/// Sized to amortise the SPSC drain cost while staying well below the
/// resampler's chunk size (so each `next_chunk` produces at most one
/// resampler chunk's worth of work).
const CPAL_DRAIN_CHUNK_SAMPLES: usize = 2048;

/// How long [`AudioSource::next_chunk`] sleeps when the ring buffer is
/// empty before retrying. Short enough that ~5 s of idle silence is
/// detected within one window (100 ms) of slack.
const CPAL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One-second ring-buffer at the worst common configuration we expect
/// (192 kHz × 8 channels). Sized in samples (not frames) because the cpal
/// callback delivers interleaved samples.
const CPAL_RING_CAPACITY_SAMPLES: usize = 192_000 * 8;

/// Production [`AudioSource`] backed by a `cpal` input stream.
///
/// Opens the default input device (or the named device matching `--device`),
/// builds a stream at the device's default config, and feeds the f32-coerced
/// samples through a lock-free SPSC ring buffer to the consumer side. The
/// cpal callback runs on cpal's own audio thread and must never block;
/// resampling/idle detection/writing all happen on the consumer side.
pub struct CpalAudioSource {
    consumer: HeapCons<f32>,
    sample_rate: u32,
    channels: u16,
    stream_error: Arc<Mutex<Option<String>>>,
    /// Held to keep the cpal stream alive. Dropped before the writer is
    /// finalised so all in-flight callback samples have flushed through
    /// the ring buffer.
    _stream: Stream,
}

impl CpalAudioSource {
    /// Opens the default input device (or the device matching
    /// `device_name`, if provided) and starts a stream at its native rate
    /// and channel count.
    ///
    /// `device_name` matching is exact (case-sensitive) against
    /// `Device::name()` — cpal reports platform-native names which differ
    /// across macOS/Linux/Windows, so users get an error listing every
    /// detected device when no match is found.
    pub fn new(device_name: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let device = match device_name {
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow!("No default input device available on this host"))?,
            Some(name) => find_input_device(&host, name)?,
        };
        let resolved_name = device.description().map_or_else(
            |_| "<unnamed device>".to_string(),
            |desc| desc.name().to_string(),
        );
        let supported = device
            .default_input_config()
            .with_context(|| format!("Failed to query default input config for {resolved_name}"))?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        let rb = HeapRb::<f32>::new(CPAL_RING_CAPACITY_SAMPLES);
        let (mut producer, consumer) = rb.split();
        let stream_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let error_clone = stream_error.clone();
        let err_fn = move |err: cpal::StreamError| {
            if let Ok(mut slot) = error_clone.lock() {
                *slot = Some(err.to_string());
            }
        };

        let stream = match sample_format {
            SampleFormat::F32 => device
                .build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        producer.push_slice(data);
                    },
                    err_fn,
                    None,
                )
                .with_context(|| format!("Failed to build f32 input stream on {resolved_name}"))?,
            SampleFormat::I16 => device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        for sample in data {
                            let _ = producer.try_push(sample.to_float_sample());
                        }
                    },
                    err_fn,
                    None,
                )
                .with_context(|| format!("Failed to build i16 input stream on {resolved_name}"))?,
            SampleFormat::U16 => device
                .build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        for sample in data {
                            let _ = producer.try_push(sample.to_float_sample());
                        }
                    },
                    err_fn,
                    None,
                )
                .with_context(|| format!("Failed to build u16 input stream on {resolved_name}"))?,
            other => anyhow::bail!(
                "Unsupported cpal sample format {other:?} on {resolved_name} \
                 (only F32, I16, U16 are wired up — file an issue if you need others)"
            ),
        };
        stream
            .play()
            .with_context(|| format!("Failed to start input stream on {resolved_name}"))?;

        Ok(Self {
            consumer,
            sample_rate,
            channels,
            stream_error,
            _stream: stream,
        })
    }

    fn take_stream_error(&self) -> Option<String> {
        self.stream_error.lock().ok().and_then(|mut s| s.take())
    }
}

impl AudioSource for CpalAudioSource {
    fn next_chunk(&mut self) -> Option<Vec<f32>> {
        if let Some(err) = self.take_stream_error() {
            tracing::warn!("cpal stream error: {err}");
            return None;
        }
        // Poll until samples arrive — cpal callbacks deliver in bursts at
        // the device's buffer cadence. Returning empty Vecs every poll
        // would burn CPU on the consumer side without producing useful
        // work.
        let mut buf = vec![0.0_f32; CPAL_DRAIN_CHUNK_SAMPLES];
        loop {
            let popped = self.consumer.pop_slice(&mut buf);
            if popped > 0 {
                buf.truncate(popped);
                return Some(buf);
            }
            if let Some(err) = self.take_stream_error() {
                tracing::warn!("cpal stream error: {err}");
                return None;
            }
            std::thread::sleep(CPAL_POLL_INTERVAL);
        }
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

fn find_input_device(host: &cpal::Host, name: &str) -> Result<<cpal::Host as HostTrait>::Device> {
    let devices = host
        .input_devices()
        .context("Failed to enumerate input devices")?;
    let mut available: Vec<String> = Vec::new();
    for device in devices {
        let device_name = device.description().map_or_else(
            |_| "<unnamed device>".to_string(),
            |desc| desc.name().to_string(),
        );
        if device_name == name {
            return Ok(device);
        }
        available.push(device_name);
    }
    Err(anyhow!(
        "Input device {name:?} not found. Available: {available:?}"
    ))
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

    #[test]
    #[ignore = "requires a working audio input device (local hardware only)"]
    fn cpal_default_input_produces_samples() -> Result<()> {
        let mut src = CpalAudioSource::new(None)?;
        assert!(src.sample_rate() > 0);
        assert!(src.channels() > 0);
        let chunk = src
            .next_chunk()
            .expect("default input should produce at least one chunk");
        assert!(!chunk.is_empty(), "default input chunk should not be empty");
        Ok(())
    }

    #[test]
    fn file_source_decodes_f32_fixtures() -> Result<()> {
        // Exercise the SampleFormat::Float branch in read_all_samples_as_f32.
        // Most capture-side cpal configs are f32, so a fixture in that format
        // is a realistic stand-in.
        let tmp = TempDir::new()?;
        let path = tmp.path().join("float.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec)?;
        for s in [0.0_f32, 0.25, -0.25, 0.5, -0.5] {
            writer.write_sample(s)?;
        }
        writer.finalize()?;

        let mut src = FileAudioSource::from_path(&path, 16)?;
        let chunk = src.next_chunk().expect("chunk");
        assert_eq!(chunk.len(), 5);
        assert!((chunk[0] - 0.0).abs() < 1e-6);
        assert!((chunk[1] - 0.25).abs() < 1e-6);
        assert!((chunk[2] + 0.25).abs() < 1e-6);
        assert!((chunk[3] - 0.5).abs() < 1e-6);
        assert!((chunk[4] + 0.5).abs() < 1e-6);
        Ok(())
    }

    #[test]
    fn file_source_open_missing_path_errors() {
        let Err(err) = FileAudioSource::from_path("/this/path/does/not/exist.wav", 16) else {
            panic!("expected open of missing file to error");
        };
        assert!(
            err.to_string().contains("Failed to open fixture WAV"),
            "got: {err}"
        );
    }

    #[test]
    fn i32_pcm_scale_matches_bit_depth() {
        // 16-bit: divisor is 2^15 = 32768
        assert!((i32_pcm_scale(16) - 32768.0).abs() < f32::EPSILON);
        // 24-bit: divisor is 2^23 = 8_388_608
        assert!((i32_pcm_scale(24) - 8_388_608.0).abs() < f32::EPSILON);
        // 32-bit: divisor is 2^31
        assert!((i32_pcm_scale(32) - (1u64 << 31) as f32).abs() < f32::EPSILON);
        // 0-bit nonsense input clamps to shift = 0, divisor = 1 (no panic)
        assert!((i32_pcm_scale(0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cpal_unknown_device_lists_alternatives() {
        let result = CpalAudioSource::new(Some(
            "this-device-name-definitely-does-not-exist-on-anyone-system",
        ));
        let Err(err) = result else {
            panic!("expected unknown device to error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("not found"),
            "error message should say 'not found': {msg}"
        );
        assert!(
            msg.contains("Available"),
            "error message should list available devices: {msg}"
        );
    }
}
