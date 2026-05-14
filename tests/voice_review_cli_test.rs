//! CLI smoke tests for `voice review`.
//!
//! Subprocess-observable concerns only: `--help` shape and argument
//! parsing. The reconciliation pipeline is covered library-level by
//! [`tests/voice_review_test.rs`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_omni-dev"))
}

#[test]
fn voice_review_help_renders() {
    let out = bin().args(["voice", "review", "--help"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Reconciles a session"),
        "help text missing summary line: {stdout}"
    );
    assert!(
        stdout.contains("SESSION_ID"),
        "help text missing SESSION_ID positional: {stdout}"
    );
    assert!(
        stdout.contains("--what"),
        "help text missing --what flag: {stdout}"
    );
    for variant in ["transcript", "todos", "decisions", "all"] {
        assert!(
            stdout.contains(variant),
            "help text missing --what variant {variant}: {stdout}"
        );
    }
}

#[test]
fn voice_review_rejects_unknown_what_value() {
    let out = bin()
        .args(["voice", "review", "demo", "--what", "garbage"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "should fail on unknown --what value");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("invalid value"),
        "expected clap value-validation error, got: {stderr}"
    );
}

#[test]
fn voice_review_executes_against_real_session_directory() {
    // End-to-end coverage of ReviewCommand::execute → VoiceCommand
    // dispatch → run_review → file writes. Library-level tests inject
    // FixedClock + CountingUlidRng directly into run_review; this one
    // exercises the production SystemClock / SystemUlidRng path via
    // the actual binary.
    let tmp = tempfile::TempDir::new().unwrap();
    let session_dir = tmp.path().join("demo");
    std::fs::create_dir_all(&session_dir).unwrap();

    // Seed a single fresh todo so reconciliation produces non-empty
    // todos.md without triggering any TTL events (which would be
    // ordering-dependent under SystemUlidRng).
    let event = serde_json::json!({
        "event_id": "01JX0000000000000000000001",
        "ts": "2099-01-01T00:00:00Z",
        "reflection_id": "01JX0000000000000000000099",
        "provenance": {
            "transcript_span": {
                "start_event_id": "01JX0000000000000000000A01",
                "end_event_id":   "01JX0000000000000000000A02"
            },
            "model": "test",
            "prompt_version": "v1"
        },
        "event_type": "item.create",
        "payload": {
            "item_id": "01JX0000000000000000001001",
            "class": "todo",
            "text": "future todo",
            "priority": "high",
            "valid_until": "2099-12-31T00:00:00Z"
        }
    });
    std::fs::write(
        session_dir.join("events.jsonl"),
        serde_json::to_string(&event).unwrap() + "\n",
    )
    .unwrap();

    let out = bin()
        .env("OMNI_DEV_VOICE_ROOT", tmp.path())
        .args(["voice", "review", "demo"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let todos = std::fs::read_to_string(session_dir.join("todos.md")).unwrap();
    assert!(todos.contains("future todo"), "got: {todos}");
    let decisions = std::fs::read_to_string(session_dir.join("decisions.md")).unwrap();
    assert!(decisions.contains("# Decisions"), "got: {decisions}");
}
