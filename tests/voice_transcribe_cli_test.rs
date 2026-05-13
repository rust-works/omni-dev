//! End-to-end snapshot tests for the `omni-dev voice transcribe` CLI.
//!
//! Spawns the compiled binary against the committed
//! `tests/fixtures/voice/short_en.wav` fixture and pins both `--format
//! jsonl` and `--format md` outputs. Distinct from the trait-level test
//! in `tests/voice_transcribe_test.rs`, which exercises `MockTranscriber`
//! through a deterministic-RNG seam — this test goes through the
//! production factory (`create_default_transcriber`), which seeds the
//! mock with `SystemUlidRng`. The `event_id` ULIDs are redacted via
//! `insta`'s filter so the snapshot is byte-stable across runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests/fixtures/voice/short_en.wav")
}

fn run_transcribe(format: &str) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args([
            "voice",
            "transcribe",
            fixture_path().to_str().unwrap(),
            "--format",
            format,
        ])
        .output()
        .expect("failed to run omni-dev voice transcribe");
    assert!(
        output.status.success(),
        "voice transcribe --format {format} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("stdout was not UTF-8")
}

#[test]
fn voice_transcribe_jsonl_format() {
    let out = run_transcribe("jsonl");
    insta::with_settings!({
        filters => vec![
            // The production factory seeds MockTranscriber with
            // SystemUlidRng, so event_id is real and varies per run.
            // Redact it to keep the snapshot byte-stable.
            (r#""event_id":"[0-9A-Z]{26}""#, r#""event_id":"<ULID>""#),
        ],
    }, {
        insta::assert_snapshot!("voice_transcribe_cli_jsonl", out);
    });
}

#[test]
fn voice_transcribe_md_format() {
    let out = run_transcribe("md");
    insta::assert_snapshot!("voice_transcribe_cli_md", out);
}

#[test]
fn voice_transcribe_rejects_missing_wav() {
    // Exercises the WAV-load error path in TranscribeCommand::execute. The
    // error message comes from VecAudioInput::from_wav_path and points the
    // user back at `voice capture` for normalised audio.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args([
            "voice",
            "transcribe",
            "/nonexistent/path/should-not-exist.wav",
            "--format",
            "jsonl",
        ])
        .output()
        .expect("failed to run omni-dev voice transcribe");
    assert!(
        !output.status.success(),
        "missing WAV should exit non-zero; got stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Failed to open WAV"),
        "stderr should surface the WAV-open error, got: {stderr}"
    );
}
