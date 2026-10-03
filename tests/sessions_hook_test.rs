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

const JOURNAL_ID: &str = "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4c";

fn journal_hook(event: &str) -> String {
    format!(
        r#"{{"session_id":"{JOURNAL_ID}","cwd":"/tmp","hook_event_name":"{event}","prompt":"PROMPT_CONTENT_SECRET","tool_input":{{"command":"TOOL_CONTENT_SECRET"}}}}"#
    )
}

fn journal_path(dir: &std::path::Path, agent: &str) -> std::path::PathBuf {
    dir.join("sessions")
        .join(agent)
        .join(format!("{JOURNAL_ID}.jsonl"))
}

#[test]
fn the_sink_journals_an_event_before_the_daemon_ever_answers() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    for agent in ["claude", "codex"] {
        let output = run_sink(&socket, dir.path(), agent, &journal_hook("Stop"));
        assert_silent_success(&output);

        let path = journal_path(dir.path(), agent);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(!text.contains("CONTENT_SECRET"), "{text}");
        let record: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(record["v"], 1);
        assert_eq!(record["agent"], agent);
        assert_eq!(record["session_id"], JOURNAL_ID);
        assert_eq!(record["op"], "observe");
        assert_eq!(record["event"], "stop");
        assert_eq!(record["cwd"], "/tmp");

        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&dir.path().join("sessions")), 0o700);
    }
}

#[test]
fn the_post_carries_the_stamp_the_journal_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut writer = stream;
        writer.write_all(b"{\"ok\":true,\"payload\":{}}\n").unwrap();
        let _ = tx.send(request);
    });

    let output = run_sink(&socket, dir.path(), "claude", &journal_hook("PreToolUse"));
    assert_silent_success(&output);

    let request: serde_json::Value =
        serde_json::from_str(rx.recv_timeout(Duration::from_secs(10)).unwrap().trim()).unwrap();
    let text = std::fs::read_to_string(journal_path(dir.path(), "claude")).unwrap();
    let record: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(request["payload"]["stamp"]["seq"], record["seq"]);
    assert_eq!(request["payload"]["stamp"]["ts"], record["ts"]);
}

#[test]
fn a_journal_write_failure_neither_blocks_the_post_nor_prints_nor_fails() {
    let dir = tempfile::tempdir().unwrap();
    // The journal root is a file: nothing can be created under it.
    std::fs::write(dir.path().join("sessions"), "").unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut writer = stream;
        writer.write_all(b"{\"ok\":true,\"payload\":{}}\n").unwrap();
        let _ = tx.send(request);
    });

    let output = run_sink(
        &socket,
        dir.path(),
        "claude",
        &journal_hook("PermissionRequest"),
    );
    assert_silent_success(&output);
    assert!(
        output.stderr.is_empty(),
        "the sink must stay quiet on stderr too: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The event still reached the daemon.
    let request: serde_json::Value =
        serde_json::from_str(rx.recv_timeout(Duration::from_secs(10)).unwrap().trim()).unwrap();
    assert_eq!(request["op"], "observe");
    assert_eq!(request["payload"]["session_id"], JOURNAL_ID);
}

#[test]
fn a_session_end_is_journaled_so_one_missed_while_the_daemon_was_down_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let output = run_sink(&socket, dir.path(), "claude", &journal_hook("SessionEnd"));
    assert_silent_success(&output);
    let text = std::fs::read_to_string(journal_path(dir.path(), "claude")).unwrap();
    let record: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(record["op"], "end");
    assert!(!text.contains("CONTENT_SECRET"));
}

#[test]
fn an_id_that_is_not_a_uuid_is_never_turned_into_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let hostile = r#"{"session_id":"../../../escape","cwd":"/tmp","hook_event_name":"Stop"}"#;
    let output = run_sink(&socket, dir.path(), "claude", hostile);
    assert_silent_success(&output);
    assert!(!dir.path().join("sessions").exists());
    assert!(!dir.path().parent().unwrap().join("escape").exists());
}
