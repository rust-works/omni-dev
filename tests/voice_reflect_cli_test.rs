//! CLI smoke tests for `voice reflect`.
//!
//! Limited to subprocess-observable concerns: argument parsing, `--help`
//! shape, and error surface. The full reflect pipeline (transcript →
//! mocked AI → events.jsonl) is covered library-level by
//! [`tests/voice_reflect_test.rs`], which avoids needing a test backdoor
//! into the production AI dispatch.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_omni-dev"))
}

#[test]
fn voice_reflect_help_renders() {
    let out = bin().args(["voice", "reflect", "--help"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Reflects on a transcript"),
        "help text missing summary line: {stdout}"
    );
    assert!(
        stdout.contains("--session"),
        "help text missing --session flag: {stdout}"
    );
    assert!(
        stdout.contains("TRANSCRIPT"),
        "help text missing TRANSCRIPT positional: {stdout}"
    );
}

#[test]
fn voice_reflect_rejects_both_transcript_and_session() {
    let out = bin()
        .args([
            "voice",
            "reflect",
            "/tmp/does-not-need-to-exist.jsonl",
            "--session",
            "x",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "should fail when both are passed");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("either a transcript path or --session"),
        "expected resolve_source error, got: {stderr}"
    );
}

#[test]
fn voice_reflect_with_nonexistent_path_errors() {
    let out = bin()
        .args([
            "voice",
            "reflect",
            "/definitely/does/not/exist/transcript.jsonl",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("opening transcript")
            || stderr.contains("No such file")
            || stderr.contains("does not exist")
            || stderr.contains("transcript"),
        "stderr should reference the missing transcript, got: {stderr}"
    );
}
