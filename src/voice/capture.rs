//! End-to-end capture pipeline orchestrator.
//!
//! Populated in step 6: glues an [`AudioSource`](super::audio) implementation
//! through mixdown → resample → idle-detection → trailing-silence trim →
//! [`hound`] writer, with signal-driven termination wired up in step 8.
