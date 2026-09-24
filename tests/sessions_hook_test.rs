#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end check, through the real binary, that the `sessions hook` sink
//! never answers a `PermissionRequest` or an `Elicitation` (#1915).
//!
//! Claude Code reads a `PermissionRequest` hook's stdout as a decision: JSON
//! carrying `"behavior": "allow"` approves the tool call on the user's behalf.
//! An `Elicitation` hook's stdout can likewise answer the MCP server's question.
//! The sink must therefore print nothing and exit 0 whatever happens — a sink
//! that ever echoed a reply would be a security bug, not a state bug. Only a
//! spawned subprocess sees the real stdout, so the pure mapping tests in
//! `src/cli/sessions.rs` cannot pin this.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const PERMISSION_REQUEST: &str = r#"{"session_id":"s1","cwd":"/tmp","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}"#;

const ELICITATION: &str = r#"{"session_id":"s1","cwd":"/tmp","hook_event_name":"Elicitation","server_name":"srv","elicitation_prompt":"Continue?"}"#;

fn run_sink(socket: &std::path::Path, home: &std::path::Path, agent: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["sessions", "hook", "--agent", agent, "--socket"])
        .arg(socket)
        .env("HOME", home)
        .env("OMNI_DEV_LOG_DISABLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn assert_silent_success(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "the sink must always exit 0");
    assert!(
        output.stdout.is_empty(),
        "the sink must print nothing on an answerable event, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn permission_request_sink_is_silent_when_the_daemon_replies_with_a_decision() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    // A fake daemon that answers with a payload shaped like a permission
    // decision, and hands back the request line it received. The result comes
    // back over a channel with a deadline, so a sink that never connects fails
    // the test instead of leaving it blocked in `accept`.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut writer = stream;
        writer
            .write_all(
                br#"{"ok":true,"payload":{"behavior":"allow","hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}}"#,
            )
            .unwrap();
        writer.write_all(b"\n").unwrap();
        let _ = tx.send(request);
    });

    let output = run_sink(&socket, dir.path(), "claude", PERMISSION_REQUEST);
    assert_silent_success(&output);

    // The sink did report the wait, so the silence is not a skipped send.
    let request = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the sink never reached the fake daemon");
    let request: serde_json::Value = serde_json::from_str(request.trim()).unwrap();
    assert_eq!(request["service"], "sessions");
    assert_eq!(request["op"], "observe");
    assert_eq!(
        request["payload"]["event"]["notification"],
        "permission_prompt"
    );
}

#[test]
fn answerable_events_are_silent_with_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    for agent in ["claude", "codex"] {
        for input in [PERMISSION_REQUEST, ELICITATION] {
            let output = run_sink(&socket, dir.path(), agent, input);
            assert_silent_success(&output);
        }
    }
}
