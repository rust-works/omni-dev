//! Safe FFI wrapper around the vendored pure-C Voxtral Realtime ASR engine
//! (`antirez/voxtral.c`, MIT — see `vendor/README.md`).
//!
//! This crate is the FFI trust boundary for #933 / [ADR-0037]: all `unsafe`
//! lives here, behind an RAII-safe API, so the root `omni-dev` crate keeps
//! `unsafe_code = "deny"`. It is compiled only on `cfg(not(target_os =
//! "windows"))` (the `build.rs` fails loudly on Windows).
//!
//! The public surface mirrors the engine's streaming session API: load a model
//! ([`VoxCtx::load`]), open a stream ([`VoxCtx::stream`]), feed PCM
//! ([`VoxStream::feed`]), and drain decoded token strings ([`VoxStream::get`]),
//! ending with [`VoxStream::finish`]. Real inference needs the ~8.9 GB bf16
//! weights staged on a matching host and is therefore uncovered-by-design in CI
//! (ADR-0037); the construction/error-mapping paths are unit-tested without the
//! model.
//!
//! [ADR-0037]: ../../docs/adrs/adr-0037.md

// The raw bindgen output: non-Rust-style names, unused decls, etc.
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all,
    clippy::pedantic
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

use std::ffi::{c_char, CStr, CString};
use std::fmt;
use std::marker::PhantomData;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

/// Errors from the Voxtral engine boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoxError {
    /// `vox_load` returned NULL — the model directory is missing or its
    /// `consolidated.safetensors` could not be opened / parsed.
    Load { dir: String },
    /// The model path contained an interior NUL byte and is not a valid C
    /// string.
    NulPath,
    /// `vox_stream_init` returned NULL.
    StreamInit,
    /// `vox_stream_feed` failed (negative return), or the sample count did not
    /// fit in a C `int`.
    Feed(i32),
    /// `vox_stream_finish` failed (negative return).
    Finish(i32),
    /// `vox_stream_flush` failed (negative return).
    Flush(i32),
}

impl fmt::Display for VoxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load { dir } => write!(f, "failed to load Voxtral model from {dir:?}"),
            Self::NulPath => write!(f, "model path contains an interior NUL byte"),
            Self::StreamInit => write!(f, "failed to initialise Voxtral stream"),
            Self::Feed(rc) => write!(f, "vox_stream_feed failed (rc={rc})"),
            Self::Finish(rc) => write!(f, "vox_stream_finish failed (rc={rc})"),
            Self::Flush(rc) => write!(f, "vox_stream_flush failed (rc={rc})"),
        }
    }
}

impl std::error::Error for VoxError {}

/// An owned, loaded Voxtral model context. Frees the native context on drop.
#[derive(Debug)]
pub struct VoxCtx {
    ptr: NonNull<ffi::vox_ctx_t>,
}

// The engine context is not internally synchronised; a single context must not
// be driven from two threads at once. It is fine to move between threads.
unsafe impl Send for VoxCtx {}

impl VoxCtx {
    /// Load a model from `model_dir` (which must contain
    /// `consolidated.safetensors`). Returns [`VoxError::Load`] if the engine
    /// cannot open or parse the weights.
    pub fn load(model_dir: &Path) -> Result<Self, VoxError> {
        let c_dir =
            CString::new(model_dir.as_os_str().as_bytes()).map_err(|_| VoxError::NulPath)?;
        // SAFETY: `c_dir` is a valid NUL-terminated C string that outlives the
        // call; `vox_load` either returns a heap context we now own or NULL.
        let ptr = unsafe { ffi::vox_load(c_dir.as_ptr()) };
        NonNull::new(ptr)
            .map(|ptr| Self { ptr })
            .ok_or_else(|| VoxError::Load {
                dir: model_dir.display().to_string(),
            })
    }

    /// Set the decoder delay (lookahead) in milliseconds. Lower trades accuracy
    /// for latency; the spike's sweet spot is ~240–480 ms.
    pub fn set_delay(&mut self, delay_ms: i32) {
        // SAFETY: `self.ptr` is a live context for the lifetime of `self`.
        unsafe { ffi::vox_set_delay(self.ptr.as_ptr(), delay_ms) }
    }

    /// Open a streaming transcription session borrowing this context.
    pub fn stream(&self) -> Result<VoxStream<'_>, VoxError> {
        // SAFETY: `self.ptr` is a live context; the returned stream borrows it.
        let ptr = unsafe { ffi::vox_stream_init(self.ptr.as_ptr()) };
        NonNull::new(ptr)
            .map(|ptr| VoxStream {
                ptr,
                _ctx: PhantomData,
            })
            .ok_or(VoxError::StreamInit)
    }
}

impl Drop for VoxCtx {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was returned by `vox_load` and is freed exactly
        // once, here.
        unsafe { ffi::vox_free(self.ptr.as_ptr()) }
    }
}

/// A streaming transcription session. Borrows its [`VoxCtx`] (so the context
/// cannot be freed while a stream is live) and frees the native stream on drop.
#[derive(Debug)]
pub struct VoxStream<'ctx> {
    ptr: NonNull<ffi::vox_stream>,
    _ctx: PhantomData<&'ctx VoxCtx>,
}

unsafe impl Send for VoxStream<'_> {}

impl VoxStream<'_> {
    /// Minimum wall-clock time between encoder runs, in seconds. Lower is more
    /// responsive (more GPU overhead); higher batches more (more latency).
    pub fn set_processing_interval(&mut self, seconds: f32) {
        // SAFETY: `self.ptr` is a live stream for the lifetime of `self`.
        unsafe { ffi::vox_set_processing_interval(self.ptr.as_ptr(), seconds) }
    }

    /// Feed mono 16 kHz f32 PCM samples into the stream.
    pub fn feed(&mut self, samples: &[f32]) -> Result<(), VoxError> {
        let n: i32 = samples.len().try_into().map_err(|_| VoxError::Feed(-1))?;
        // SAFETY: `samples` is valid for `n` reads; the engine copies what it
        // needs and does not retain the pointer past the call.
        let rc = unsafe { ffi::vox_stream_feed(self.ptr.as_ptr(), samples.as_ptr(), n) };
        if rc < 0 {
            Err(VoxError::Feed(rc))
        } else {
            Ok(())
        }
    }

    /// Signal end of audio: triggers right-padding, final encoder chunks, and
    /// any remaining token generation. The stream should not be fed afterward.
    pub fn finish(&mut self) -> Result<(), VoxError> {
        // SAFETY: `self.ptr` is a live stream.
        let rc = unsafe { ffi::vox_stream_finish(self.ptr.as_ptr()) };
        if rc < 0 {
            Err(VoxError::Finish(rc))
        } else {
            Ok(())
        }
    }

    /// Force the encoder to process buffered audio without ending the stream
    /// (useful at a detected silence boundary).
    pub fn flush(&mut self) -> Result<(), VoxError> {
        // SAFETY: `self.ptr` is a live stream.
        let rc = unsafe { ffi::vox_stream_flush(self.ptr.as_ptr()) };
        if rc < 0 {
            Err(VoxError::Flush(rc))
        } else {
            Ok(())
        }
    }

    /// Drain up to `max` pending decoded token strings, copied into owned
    /// `String`s. Returns fewer than `max` (possibly empty) when less is
    /// pending.
    pub fn get(&mut self, max: usize) -> Vec<String> {
        let cap = i32::try_from(max).unwrap_or(i32::MAX);
        let mut slots: Vec<*const c_char> = vec![std::ptr::null(); cap as usize];
        // SAFETY: `slots` has room for `cap` pointers; `vox_stream_get` writes
        // at most `cap` of them and returns how many.
        let n = unsafe { ffi::vox_stream_get(self.ptr.as_ptr(), slots.as_mut_ptr(), cap) };
        let n = usize::try_from(n).unwrap_or(0);
        slots
            .iter()
            .take(n)
            .filter(|p| !p.is_null())
            .map(|&p| {
                // SAFETY: each non-NULL pointer is a valid NUL-terminated string
                // owned by the stream until `vox_stream_free`; we copy it now.
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            })
            .collect()
    }
}

impl Drop for VoxStream<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was returned by `vox_stream_init` and is freed
        // exactly once, here, before the borrowed `VoxCtx` can be dropped.
        unsafe { ffi::vox_stream_free(self.ptr.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These run without the ~8.9 GB model: they exercise the link + the
    // construction/error-mapping paths only (real inference is
    // uncovered-by-design per ADR-0037).

    #[test]
    fn load_missing_model_dir_maps_to_load_error() {
        let err = VoxCtx::load(Path::new("/voxtral-sys/definitely/not/a/model")).unwrap_err();
        assert!(matches!(err, VoxError::Load { .. }), "got {err:?}");
        // The error carries the offending directory for actionable messages.
        assert!(err.to_string().contains("not/a/model"), "got {err}");
    }

    #[test]
    fn interior_nul_path_maps_to_nul_error() {
        let p = Path::new("voxtral\0model");
        assert_eq!(VoxCtx::load(p).unwrap_err(), VoxError::NulPath);
    }

    #[test]
    fn error_display_is_actionable() {
        assert!(VoxError::Feed(-3).to_string().contains("rc=-3"));
        assert!(VoxError::StreamInit.to_string().contains("stream"));
    }
}
