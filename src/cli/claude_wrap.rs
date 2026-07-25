//! `omni-dev claude-wrap` — a transparent wrapper around the `claude` process,
//! teeing its `--output-format stream-json` stdio to the daemon's `sessions`
//! service as the authoritative Feed 4.
//!
//! The Claude Code VS Code extension launches Claude through whatever executable
//! its `claudeCode.claudeProcessWrapper` setting names, as
//! `<wrapper> <real-cmd> <real-args…>`. Pointing that at this command puts us in
//! the stream the extension itself reads, which is the only place the exact
//! session state — in particular the `can_use_tool` permission prompt — is
//! visible. See ADR-0057, and [`crate::sessions::stream`] for the state machine.
//!
//! **Fail-open is the hard rule.** This sits in Claude's launch path, so the
//! worst case must be "lose state visibility", never "Claude won't launch". The
//! byte-forwarding path therefore never awaits the parser, the daemon, or
//! anything else: lines are handed to the observer through a *bounded* channel
//! with [`try_send`](tokio::sync::mpsc::Sender::try_send) and dropped when it is
//! full, and every reporting error is swallowed exactly as the `sessions hook`
//! sink swallows its own.
//!
//! Nothing is logged or persisted. The observer reads only the state, the
//! `session_id`, the `cwd` and the model out of the stream, and reports them to
//! the daemon's existing `0600` Unix socket.

use std::io::IsTerminal;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::DaemonEnvelope;
use crate::daemon::server;
use crate::sessions::stream::{Direction, StreamTracker};

/// The `sessions` service routing key on the daemon control socket.
const SERVICE: &str = "sessions";

/// How long a fire-and-forget report waits for the daemon before giving up.
/// Short, and on a task that no byte ever waits behind.
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the current state is re-reported while the child lives.
///
/// Comfortably inside the registry's 300s session TTL, so a session that sits
/// idle at the prompt for an hour never ages out. This is the wrapper's bonus
/// over the hook and transcript feeds: liveness bounded by the real process
/// lifetime rather than by observed activity.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// How many teed lines may be in flight before further ones are dropped.
/// Generous for the two-second worst case of a slow daemon, and bounded so a
/// wedged observer can never grow memory or slow the child's I/O.
const TEE_CAPACITY: usize = 256;

/// Longest stream-json line the tee will reassemble; longer lines are forwarded
/// verbatim like every other byte but not parsed. Matches the daemon control
/// socket's own line ceiling.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Read-buffer size for each direction of the byte pump.
const PUMP_BUF_BYTES: usize = 64 * 1024;

/// Exit status used when the child is killed by a signal, mirroring the shell's
/// `128 + signo` convention.
const SIGNAL_EXIT_BASE: i32 = 128;

/// Runs a command transparently, reporting the Claude session state it streams.
#[derive(Parser)]
pub struct ClaudeWrapCommand {
    /// Path to the daemon control socket. Defaults to the standard per-user path.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// The command to run and its arguments, after a `--` separator.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD"
    )]
    pub argv: Vec<String>,
}

impl ClaudeWrapCommand {
    /// Executes the wrapper: runs the given command, forwards its stdio
    /// verbatim, and exits with the child's own status.
    ///
    /// This never returns normally on the wrapped path — it calls
    /// [`std::process::exit`] so the child's exit code survives, which the
    /// framework's `Result`-to-0/1 collapse in `main` could not express. The
    /// consequence is that a `claude-wrap` invocation writes no request-log
    /// record, which is deliberate: it is a process shim, not a user command.
    pub async fn execute(self) -> Result<()> {
        let Some((program, args)) = self.argv.split_first() else {
            bail!(
                "claude-wrap needs a command to run, e.g. \
                 `omni-dev claude-wrap -- claude --output-format stream-json`"
            );
        };

        // An interactive launch is not the stream-json protocol, so there is
        // nothing to observe and no reason to sit in the middle of it: replace
        // this process with the child outright, which is as transparent as it
        // gets (same pid, same terminal, same job control, same exit status).
        if std::io::stdout().is_terminal() {
            return Err(exec_replace(program, args));
        }

        let code = wrap(program, args, self.socket).await?;
        std::process::exit(code)
    }
}

/// Replaces this process with `program`, returning the error only if the `exec`
/// itself failed (on success it never returns).
fn exec_replace(program: &str, args: &[String]) -> anyhow::Error {
    let error = std::process::Command::new(program).args(args).exec();
    anyhow::Error::new(error).context(format!("failed to exec {program}"))
}

/// Wraps `program`, joining it to this process's own stdin and stdout.
async fn wrap(program: &str, args: &[String], socket: Option<PathBuf>) -> Result<i32> {
    wrap_io(
        program,
        args,
        socket,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

/// Spawns the child with piped stdio, pumps `input` and `output` through it
/// verbatim while teeing both directions to the observer, and returns the
/// child's exit code.
///
/// Generic over the two endpoints so the wrapping can be tested against a real
/// child process without touching the test runner's own stdio — which is also
/// the only way to assert that the forwarding is byte-for-byte lossless.
async fn wrap_io<R, W>(
    program: &str,
    args: &[String],
    socket: Option<PathBuf>,
    input: R,
    output: W,
) -> Result<i32>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // stderr is inherited and env is left untouched, so the child sees exactly
    // the environment it would have without the wrapper. The child is
    // deliberately *not* put in its own process group (unlike the managed
    // `claude -p` subprocess in `crate::claude::ai::claude_cli`): a transparent
    // wrapper must leave the child in the caller's group or job control breaks.
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to capture the wrapped process's stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture the wrapped process's stdout"))?;
    let child_pid = child.id();

    let (tee, lines) = mpsc::channel::<(Direction, String)>(TEE_CAPACITY);
    let observer = tokio::spawn(observe(lines, socket));
    let signals = tokio::spawn(forward_signals(child_pid));

    let from_child = tokio::spawn(pump(
        child_stdout,
        output,
        Direction::FromClaude,
        tee.clone(),
    ));
    let to_child = tokio::spawn(pump(input, child_stdin, Direction::ToClaude, tee.clone()));
    drop(tee);

    // Draining the child's stdout to EOF *before* reaping is what makes the
    // wrapper lossless: every byte the child wrote reaches our stdout, exactly
    // as it would have without us in the middle.
    let _ = from_child.await;
    // Our own stdin never reaches EOF on its own, so the input pump has to be
    // cancelled; awaiting the aborted handle drops its tee sender, which is what
    // eventually lets the observer finish.
    to_child.abort();
    let _ = to_child.await;
    signals.abort();

    let status = child
        .wait()
        .await
        .context("failed to wait for the wrapped process")?;
    let _ = observer.await;
    Ok(exit_code(status))
}

/// Copies `reader` to `writer` byte-for-byte, teeing complete lines to the
/// observer as it goes.
///
/// The copy is the priority: bytes are written and flushed before the tee is
/// even attempted, and the tee is a non-blocking [`try_send`] whose failure is
/// ignored. No I/O here can be delayed by the observer or the daemon.
///
/// [`try_send`]: tokio::sync::mpsc::Sender::try_send
async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    direction: Direction,
    tee: mpsc::Sender<(Direction, String)>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; PUMP_BUF_BYTES];
    let mut line = Vec::new();
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let chunk = &buffer[..read];
        if writer.write_all(chunk).await.is_err() || writer.flush().await.is_err() {
            break;
        }
        tee_chunk(chunk, direction, &tee, &mut line);
    }
    // Closing the write end propagates EOF (this is how the child learns its
    // input has ended); a failure here means the peer is already gone.
    let _ = writer.shutdown().await;
}

/// Reassembles newline-delimited lines out of a forwarded chunk and offers each
/// to the observer, dropping any that is over-long, not UTF-8, or arrives while
/// the channel is full.
fn tee_chunk(
    chunk: &[u8],
    direction: Direction,
    tee: &mpsc::Sender<(Direction, String)>,
    line: &mut Vec<u8>,
) {
    for &byte in chunk {
        if byte == b'\n' {
            if line.len() < MAX_LINE_BYTES {
                if let Ok(text) = std::str::from_utf8(line) {
                    let _ = tee.try_send((direction, text.to_string()));
                }
            }
            line.clear();
        } else if line.len() < MAX_LINE_BYTES {
            line.push(byte);
        }
    }
}

/// Consumes teed lines, tracks the session state, and reports every change to
/// the daemon — plus a keep-alive while nothing changes, and an `end` once the
/// stream is over.
async fn observe(mut lines: mpsc::Receiver<(Direction, String)>, socket: Option<PathBuf>) {
    let Ok(socket) = server::resolve_socket(socket) else {
        return;
    };
    let mut tracker = StreamTracker::new();
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    // The first tick of a tokio interval completes immediately; consume it so
    // the keep-alive does not fire before anything has been observed.
    keepalive.tick().await;

    loop {
        let observed = tokio::select! {
            line = lines.recv() => match line {
                Some((direction, text)) => tracker.observe_line(direction, &text),
                None => break,
            },
            _ = keepalive.tick() => tracker.keepalive(),
        };
        if let Some(request) = observed {
            if let Ok(payload) = serde_json::to_value(request) {
                report(&socket, "observe", payload).await;
            }
        }
    }

    if let Some(session_id) = tracker.session_id() {
        report(&socket, "end", json!({ "session_id": session_id })).await;
    }
}

/// Sends one bounded, fire-and-forget op to the daemon's `sessions` service,
/// swallowing every failure — a missing or wedged daemon must be a silent no-op.
async fn report(socket: &Path, op: &str, payload: Value) {
    let envelope = DaemonEnvelope::service(SERVICE, op, payload);
    let _ = tokio::time::timeout(REPORT_TIMEOUT, DaemonClient::new(socket).request(envelope)).await;
}

/// Relays `SIGINT`/`SIGTERM` to the child, so a caller that signals the wrapper
/// stops Claude rather than orphaning it.
async fn forward_signals(child_pid: Option<u32>) {
    let Some(pid) = child_pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return;
    };
    let pid = nix::unistd::Pid::from_raw(pid);
    let (Ok(mut terminate), Ok(mut interrupt)) = (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
    ) else {
        return;
    };
    loop {
        let signal = tokio::select! {
            _ = terminate.recv() => nix::sys::signal::Signal::SIGTERM,
            _ = interrupt.recv() => nix::sys::signal::Signal::SIGINT,
        };
        let _ = nix::sys::signal::kill(pid, signal);
    }
}

/// The exit code to leave with: the child's own, or the shell's `128 + signo`
/// when it died on a signal.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT_BASE + signal))
        .unwrap_or(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll};

    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixListener;

    use super::*;

    /// An [`AsyncWrite`] that appends into a shared buffer, so a test can assert
    /// on exactly the bytes the wrapper forwarded.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Sink {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl AsyncWrite for Sink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Runs `/bin/sh -c script` through the wrapper, returning its exit code and
    /// everything it wrote downstream.
    async fn wrap_script(script: &str, socket: Option<PathBuf>) -> (i32, String) {
        let sink = Sink::default();
        let code = wrap_io(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            socket,
            tokio::io::empty(),
            sink.clone(),
        )
        .await
        .unwrap();
        (code, sink.contents())
    }

    /// Parses a `claude-wrap` argument list the way the real CLI would.
    fn parse(args: &[&str]) -> ClaudeWrapCommand {
        let mut argv = vec!["claude-wrap"];
        argv.extend_from_slice(args);
        ClaudeWrapCommand::try_parse_from(argv).unwrap()
    }

    /// A one-shot daemon stand-in that records every envelope it is sent.
    fn fake_daemon() -> (
        tempfile::TempDir,
        PathBuf,
        Arc<tokio::sync::Mutex<Vec<Value>>>,
    ) {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    if tokio::io::BufReader::new(read)
                        .read_line(&mut line)
                        .await
                        .is_ok()
                    {
                        if let Ok(value) = serde_json::from_str::<Value>(&line) {
                            recorder.lock().await.push(value);
                        }
                    }
                    let _ = write.write_all(b"{\"ok\":true,\"payload\":{}}\n").await;
                });
            }
        });
        (dir, socket, seen)
    }

    #[test]
    fn trailing_args_are_captured_verbatim_after_a_separator() {
        let cmd = parse(&["--", "node", "cli.js", "--output-format", "stream-json"]);
        assert_eq!(
            cmd.argv,
            vec!["node", "cli.js", "--output-format", "stream-json"]
        );
        assert!(cmd.socket.is_none());
    }

    #[test]
    fn the_socket_override_is_parsed_before_the_separator() {
        let cmd = parse(&["--socket", "/tmp/d.sock", "--", "claude", "-p"]);
        assert_eq!(cmd.socket.unwrap(), PathBuf::from("/tmp/d.sock"));
        assert_eq!(cmd.argv, vec!["claude", "-p"]);
    }

    #[tokio::test]
    async fn an_empty_command_is_a_usage_error() {
        let error = parse(&[]).execute().await.unwrap_err();
        assert!(error.to_string().contains("needs a command to run"));
    }

    #[tokio::test]
    async fn the_child_is_wrapped_losslessly_and_its_exit_code_survives() {
        let (_dir, socket, _seen) = fake_daemon();
        // A partial trailing line and a non-JSON line: neither is parseable, and
        // both must still arrive downstream byte-for-byte.
        let (code, forwarded) =
            wrap_script("printf 'hello\\nnot json\\ntrailing'; exit 7", Some(socket)).await;
        assert_eq!(code, 7);
        assert_eq!(forwarded, "hello\nnot json\ntrailing");
    }

    #[tokio::test]
    async fn stream_state_transitions_reach_the_daemon() {
        let (_dir, socket, seen) = fake_daemon();
        // A minimal but real turn: announce, work, ask permission, finish.
        let script = concat!(
            r#"printf '{"type":"system","subtype":"init","session_id":"s-1","cwd":"/w"}\n';"#,
            r#"printf '{"type":"assistant","session_id":"s-1"}\n';"#,
            r#"printf '{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool"}}\n';"#,
            r#"printf '{"type":"result"}\n'"#,
        );
        let (code, _forwarded) = wrap_script(script, Some(socket)).await;
        assert_eq!(code, 0);

        let envelopes = seen.lock().await.clone();
        let states: Vec<String> = envelopes
            .iter()
            .filter(|e| e["op"] == "observe")
            .filter_map(|e| e["payload"]["event"]["stream_state"].as_str())
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            states,
            vec!["idle", "working", "waiting_for_permission", "idle"]
        );
        // Identity rides the reports, and the stream's end is announced.
        assert_eq!(envelopes[0]["service"], "sessions");
        assert_eq!(envelopes[0]["payload"]["session_id"], "s-1");
        assert_eq!(envelopes[0]["payload"]["cwd"], "/w");
        let ended = envelopes.last().unwrap();
        assert_eq!(ended["op"], "end");
        assert_eq!(ended["payload"]["session_id"], "s-1");
    }

    #[tokio::test]
    async fn a_missing_daemon_never_affects_the_child() {
        // A socket path that does not exist: every report fails silently.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let script = concat!(
            r#"printf '{"type":"system","subtype":"init","session_id":"s-1"}\n';"#,
            r#"printf 'not json at all\n';"#,
            r#"exit 3"#,
        );
        let (code, forwarded) = wrap_script(script, Some(dir.path().join("absent.sock"))).await;
        assert_eq!(code, 3);
        assert!(forwarded.ends_with("not json at all\n"));
    }

    #[tokio::test]
    async fn a_child_killed_by_a_signal_reports_the_shell_convention() {
        let (_dir, socket, _seen) = fake_daemon();
        let (code, _forwarded) = wrap_script("kill -TERM $$", Some(socket)).await;
        assert_eq!(code, SIGNAL_EXIT_BASE + 15);
    }

    #[tokio::test]
    async fn spawning_a_missing_program_is_an_error_not_a_panic() {
        let error = wrap_io(
            "/nonexistent/omni-dev-test-binary",
            &[],
            None,
            tokio::io::empty(),
            Sink::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("failed to spawn"));
    }

    #[test]
    fn over_long_and_non_utf8_lines_are_dropped_but_still_forwarded() {
        let (tee, mut lines) = mpsc::channel(4);
        let mut buffer = Vec::new();
        let mut chunk = vec![b'x'; MAX_LINE_BYTES + 1];
        chunk.push(b'\n');
        chunk.extend_from_slice(&[0xff, 0xfe, b'\n']);
        chunk.extend_from_slice(b"{\"type\":\"result\"}\n");
        tee_chunk(&chunk, Direction::FromClaude, &tee, &mut buffer);
        // Only the last (short, valid UTF-8) line is offered to the observer.
        let (direction, text) = lines.try_recv().unwrap();
        assert_eq!(direction, Direction::FromClaude);
        assert_eq!(text, r#"{"type":"result"}"#);
        assert!(lines.try_recv().is_err());
    }

    #[test]
    fn a_full_tee_drops_lines_rather_than_blocking() {
        let (tee, _lines) = mpsc::channel(1);
        let mut buffer = Vec::new();
        tee_chunk(b"a\nb\nc\n", Direction::ToClaude, &tee, &mut buffer);
        // Two of the three were dropped; nothing blocked, nothing panicked.
        assert_eq!(tee.capacity(), 0);
    }
}
