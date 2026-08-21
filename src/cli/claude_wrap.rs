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
use tokio::sync::{mpsc, watch};

use crate::claude::model_config::get_model_registry;
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

/// Longest we will hold bytes back while looking for the parameter or
/// terminator of a candidate OSC `0`/`2` title sequence, across any number of
/// `read()` calls. Bounds memory and is the fail-open backstop for the title
/// rewrite: a malformed or pathologically split sequence is flushed
/// unchanged, rather than held back indefinitely, once this is exceeded.
const MAX_OSC_SEQUENCE_BYTES: usize = 8 * 1024;

/// Environment variable that, set to a truthy value, disables the OSC
/// title rewrite entirely: Claude's own title sequence is then forwarded
/// unchanged, exactly as it was before issue #1445. An escape hatch for the
/// rare terminal/font where the rewritten title misrenders.
const NO_TITLE_REWRITE_ENV: &str = "OMNI_DEV_CLAUDE_WRAP_NO_TITLE_REWRITE";

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
        let code = self.run().await?;
        std::process::exit(code)
    }

    /// Runs the wrapped command and returns the exit code it should be reported
    /// with. Split out of [`execute`](Self::execute) so everything but the
    /// process-ending `exit` itself is testable.
    async fn run(self) -> Result<i32> {
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

        wrap(program, args, self.socket).await
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
    let (title_tx, title_rx) = watch::channel::<Option<String>>(None);
    let observer = tokio::spawn(observe(lines, socket, KEEPALIVE_INTERVAL, title_tx));
    let signals = tokio::spawn(forward_signals(child_pid));

    // The title rewrite only ever applies to the FromClaude direction — it
    // rewrites what Claude asserts about itself, never what we send it.
    let from_child_title_rx = title_rewrite_enabled().then_some(title_rx);
    let from_child = tokio::spawn(pump(
        child_stdout,
        output,
        Direction::FromClaude,
        tee.clone(),
        from_child_title_rx,
    ));
    let to_child = tokio::spawn(pump(
        input,
        child_stdin,
        Direction::ToClaude,
        tee.clone(),
        None,
    ));
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
/// `title_rx` is the one deliberate, narrow exception to "never depends on
/// the observer" (see the [`TitleRewriter`] doc comment and the ADR-0057
/// amendment for #1445): when present, each chunk is passed through
/// [`TitleRewriter`] before being written, which does a single non-blocking
/// [`watch::Receiver::borrow`] of a value the observer publishes — never an
/// `.await` on the observer or the daemon. `None` (always the `ToClaude`
/// direction, and `FromClaude` too when the rewrite is disabled) skips the
/// rewrite entirely and writes the chunk verbatim, exactly as before #1445.
///
/// A second, independent branch races [`watch::Receiver::changed`] against
/// the read: real Claude asserts its title once at startup, not on a timer,
/// so waiting only on chunks read *from Claude* would very often lose the
/// race against the observer classifying the model — with nothing left to
/// reassert and catch it later. When the model becomes known (or changes),
/// this branch calls [`TitleRewriter::resync`] to correct the cached title
/// immediately, with no new bytes from Claude required. It is still
/// non-blocking on the observer in the sense that matters: a value already
/// sitting in the channel is read instantly, and if the observer never
/// updates it, this branch simply never fires — it cannot hang the pump.
///
/// [`try_send`]: tokio::sync::mpsc::Sender::try_send
async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    direction: Direction,
    tee: mpsc::Sender<(Direction, String)>,
    mut title_rx: Option<watch::Receiver<Option<String>>>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; PUMP_BUF_BYTES];
    let mut line = Vec::new();
    let mut rewriter = TitleRewriter::default();
    loop {
        let title_changed = async {
            match title_rx.as_mut() {
                Some(rx) => rx.changed().await,
                // No receiver (ToClaude, or the rewrite is disabled): never
                // resolves, so `select!` only ever waits on the read.
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            read_result = reader.read(&mut buffer) => {
                let read = match read_result {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let chunk = &buffer[..read];
                let write_result = match &title_rx {
                    Some(rx) => {
                        let prefix = rx.borrow().clone();
                        writer
                            .write_all(&rewriter.rewrite(chunk, prefix.as_deref()))
                            .await
                    }
                    None => writer.write_all(chunk).await,
                };
                if write_result.is_err() || writer.flush().await.is_err() {
                    break;
                }
                tee_chunk(chunk, direction, &tee, &mut line);
            }
            changed = title_changed => {
                match changed {
                    Ok(()) => {
                        let Some(rx) = &title_rx else { continue };
                        let prefix = rx.borrow().clone();
                        let Some(bytes) = rewriter.resync(prefix.as_deref()) else { continue };
                        if writer.write_all(&bytes).await.is_err()
                            || writer.flush().await.is_err()
                        {
                            break;
                        }
                    }
                    // The observer's sender was dropped; stop selecting on
                    // this branch rather than spinning on a channel that
                    // will report closed forever.
                    Err(_) => title_rx = None,
                }
            }
        }
    }
    // Closing the write end propagates EOF (this is how the child learns its
    // input has ended); a failure here means the peer is already gone.
    let _ = writer.shutdown().await;
}

/// Rewrites Claude's own OSC `0`/`2` terminal-title sequence in flight,
/// prepending a colour-glyph + model-family prefix, while leaving every other
/// byte untouched (issue #1445).
///
/// **Fail-open by construction**, matching every other guarantee this module
/// makes: a candidate sequence is forwarded byte-for-byte unchanged, exactly
/// as received, whenever it
/// - is not an OSC `0` or `2` sequence (any other OSC code, or a bare `ESC`
///   not followed by `]`),
/// - is not terminated with `BEL` or the two-byte `ESC \` (ST) form,
/// - carries a title that is not valid UTF-8,
/// - exceeds [`MAX_OSC_SEQUENCE_BYTES`] before terminating, or
/// - arrives while no model is known yet (`prefix` is `None`).
///
/// State (`pending`/`title`/`code`/`phase`) persists across calls so a
/// sequence split unfavourably across a `read()` buffer boundary is still
/// recognized and rewritten correctly.
///
/// Real Claude Code asserts its title **once**, as part of its terminal-mode
/// setup at startup, not on a timer — measured empirically, not the
/// "continuously re-asserted" behavior issue #1445 assumed. Left as
/// rewrite-on-assert alone, that one assertion almost always races ahead of
/// the model becoming known (the observer needs to receive, tee, and parse
/// the `system`/`init` line first), and since there is no later reassertion
/// to catch, the title is never corrected for the rest of the session. Every
/// completed sequence is therefore also cached — regardless of whether it
/// was rewritten or forwarded unchanged — so [`Self::resync`] can re-emit a
/// corrected title the moment the model becomes known (or changes again),
/// without waiting for Claude to assert anything else.
#[derive(Debug, Default)]
struct TitleRewriter {
    /// Every raw byte seen since leaving [`Phase::Idle`] — the exact bytes
    /// flushed verbatim whenever a candidate sequence does not end up being
    /// rewritten.
    pending: Vec<u8>,
    /// The title text accumulated once past `ESC ] <code> ;`, excluding the
    /// terminator.
    title: Vec<u8>,
    /// The OSC parameter byte (`b'0'` or `b'2'`), once known.
    code: u8,
    phase: Phase,
    /// The most recently completed sequence's parameter byte, title text,
    /// and terminator — cached regardless of whether it was rewritten, so
    /// [`Self::resync`] can reformat it under a new prefix.
    last_code: Option<u8>,
    last_title: Option<Vec<u8>>,
    last_terminator: Option<Vec<u8>>,
}

/// `TitleRewriter`'s position within (or outside of) a candidate OSC title
/// sequence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not inside any candidate escape sequence; bytes pass straight through.
    #[default]
    Idle,
    /// Saw `ESC`; deciding whether `]` follows.
    Esc,
    /// Saw `ESC ]`; deciding whether a `0`/`2` title code follows.
    Open,
    /// Saw `ESC ] <code>`; deciding whether `;` follows.
    Code,
    /// Accumulating title text after `ESC ] <code> ;`, until `BEL` or ST.
    Title,
    /// Saw `ESC` while accumulating title text; deciding whether `\` (the
    /// second byte of the ST terminator) follows.
    TitleEsc,
}

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;

impl TitleRewriter {
    /// Passes `chunk` through the rewriter, returning the bytes that should
    /// actually be written downstream. `prefix` is the current
    /// `"{glyph} {label} · "` string to prepend to a rewritten title, or
    /// `None` while no model is known yet.
    fn rewrite(&mut self, chunk: &[u8], prefix: Option<&str>) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        for &byte in chunk {
            match self.phase {
                Phase::Idle => {
                    if byte == ESC {
                        self.pending.push(byte);
                        self.phase = Phase::Esc;
                    } else {
                        out.push(byte);
                    }
                }
                Phase::Esc => {
                    self.pending.push(byte);
                    if byte == b']' {
                        self.phase = Phase::Open;
                    } else {
                        self.flush_pending(&mut out);
                    }
                }
                Phase::Open => {
                    self.pending.push(byte);
                    if byte == b'0' || byte == b'2' {
                        self.code = byte;
                        self.phase = Phase::Code;
                    } else {
                        self.flush_pending(&mut out);
                    }
                }
                Phase::Code => {
                    self.pending.push(byte);
                    if byte == b';' {
                        self.phase = Phase::Title;
                    } else {
                        self.flush_pending(&mut out);
                    }
                }
                Phase::Title => {
                    self.pending.push(byte);
                    if byte == BEL {
                        self.finish(&mut out, prefix, &[BEL]);
                    } else if byte == ESC {
                        self.phase = Phase::TitleEsc;
                    } else {
                        self.title.push(byte);
                    }
                }
                Phase::TitleEsc => {
                    self.pending.push(byte);
                    if byte == b'\\' {
                        self.finish(&mut out, prefix, &[ESC, b'\\']);
                    } else {
                        // Not a valid ST terminator: the whole candidate is
                        // malformed, so flush verbatim rather than guess.
                        self.flush_pending(&mut out);
                    }
                }
            }
            if self.pending.len() > MAX_OSC_SEQUENCE_BYTES {
                self.flush_pending(&mut out);
            }
        }
        out
    }

    /// A completed sequence: rewrites it when `prefix` and a valid UTF-8
    /// title are both available, otherwise flushes the original bytes
    /// unchanged. Cached either way, for [`Self::resync`].
    fn finish(&mut self, out: &mut Vec<u8>, prefix: Option<&str>, terminator: &[u8]) {
        let rewritten = prefix.zip(std::str::from_utf8(&self.title).ok());
        match rewritten {
            Some((prefix, title)) => {
                out.push(ESC);
                out.push(b']');
                out.push(self.code);
                out.push(b';');
                out.extend_from_slice(prefix.as_bytes());
                out.extend_from_slice(title.as_bytes());
                out.extend_from_slice(terminator);
            }
            None => out.extend_from_slice(&self.pending),
        }
        self.last_code = Some(self.code);
        self.last_title = Some(self.title.clone());
        self.last_terminator = Some(terminator.to_vec());
        self.reset();
    }

    /// Re-emits the most recently completed title sequence under a new
    /// `prefix`, so a title Claude already asserted — before any model was
    /// known, or under a now-stale one — is corrected without waiting for
    /// Claude to reassert it on its own (which, empirically, it may never
    /// do again for the rest of the session). Returns `None` when nothing
    /// has completed yet, `prefix` is still unavailable, or the cached title
    /// is not valid UTF-8.
    fn resync(&self, prefix: Option<&str>) -> Option<Vec<u8>> {
        let prefix = prefix?;
        let code = self.last_code?;
        let title = std::str::from_utf8(self.last_title.as_ref()?).ok()?;
        let terminator = self.last_terminator.as_ref()?;
        let mut out = vec![ESC, b']', code, b';'];
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(title.as_bytes());
        out.extend_from_slice(terminator);
        Some(out)
    }

    /// Flushes everything buffered so far, unchanged, and returns to `Idle`.
    fn flush_pending(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
        self.reset();
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.title.clear();
        self.code = 0;
        self.phase = Phase::Idle;
    }
}

/// Whether the OSC title rewrite is enabled — the default, unless
/// [`NO_TITLE_REWRITE_ENV`] is set to a truthy value.
fn title_rewrite_enabled() -> bool {
    !std::env::var(NO_TITLE_REWRITE_ENV).is_ok_and(|v| flag_is_truthy(&v))
}

/// Whether a `NO_TITLE_REWRITE_ENV`-style flag value should be read as "on".
/// Split out from [`title_rewrite_enabled`] so the parsing itself is testable
/// without mutating process-wide environment state.
fn flag_is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Builds the `"{glyph} {label} · "` prefix to prepend to Claude's own OSC
/// title text for `model_id`, classified via the shared [`ModelRegistry`]
/// (issue #1445).
///
/// [`ModelRegistry`]: crate::claude::model_config::ModelRegistry
fn title_prefix_for_model(model_id: &str) -> String {
    let family = get_model_registry().get_model_family(model_id);
    format!("{} {} · ", family.glyph(), family.label())
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
/// `every` is the keep-alive cadence, injected so tests can drive it without
/// waiting out the real [`KEEPALIVE_INTERVAL`].
async fn observe(
    mut lines: mpsc::Receiver<(Direction, String)>,
    socket: Option<PathBuf>,
    every: Duration,
    title_tx: watch::Sender<Option<String>>,
) {
    let Ok(socket) = server::resolve_socket(socket) else {
        return;
    };
    let mut tracker = StreamTracker::new();
    let mut last_model: Option<String> = None;
    let mut keepalive = tokio::time::interval(every);
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
        // Publish the classified title prefix whenever the model changes —
        // independent of `observed`, since a mid-session model switch does
        // not necessarily move `SessionState` (issue #1445). The pump's
        // `watch::Receiver::borrow` on the other end never blocks on this.
        if tracker.model() != last_model.as_deref() {
            last_model = tracker.model().map(str::to_string);
            let prefix = last_model.as_deref().map(title_prefix_for_model);
            let _ = title_tx.send(prefix);
        }
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
        let error = parse(&[]).run().await.unwrap_err();
        assert!(error.to_string().contains("needs a command to run"));
    }

    // Note: there is deliberately no test that drives `run`/`wrap` to completion.
    // Both join the *process's own* stdin and stdout, which a test must not take
    // over — and `run`'s terminal branch would `exec`-replace the test binary
    // outright when the suite is run from an interactive shell. They are thin
    // delegations; `wrap_io` underneath them is covered directly.

    // Note: `exec_replace` is deliberately untested. On success it never returns,
    // and its failure path cannot be exercised in-process either: `exec` resets
    // `SIGPIPE` to `SIG_DFL` before the `execve`, and a failed `exec` leaves that
    // reset in place (the "broken state" its docs warn about). Calling it from a
    // test poisons the whole binary — every later test that writes to a closed
    // pipe dies on signal 13.

    #[tokio::test]
    async fn signal_forwarding_gives_up_when_there_is_no_child() {
        // A child already reaped has no pid to signal; this must return rather
        // than spin.
        forward_signals(None).await;
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

    #[tokio::test]
    async fn the_keepalive_re_reports_a_silent_session() {
        // The wrapper's TTL-liveness guarantee: a session that says nothing more
        // must keep being reported, or the registry ages it out at 5 minutes.
        let (_dir, socket, seen) = fake_daemon();
        let (tee, lines) = mpsc::channel(4);
        let (title_tx, _title_rx) = watch::channel(None);
        let observer = tokio::spawn(observe(
            lines,
            Some(socket),
            Duration::from_millis(20),
            title_tx,
        ));
        tee.send((
            Direction::FromClaude,
            r#"{"type":"system","subtype":"init","session_id":"ka-1"}"#.to_string(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        drop(tee);
        observer.await.unwrap();

        let envelopes = seen.lock().await.clone();
        let observes: Vec<_> = envelopes.iter().filter(|e| e["op"] == "observe").collect();
        // One for the state change, then repeats carrying the same state.
        assert!(observes.len() > 1, "expected keep-alives, got {observes:?}");
        for envelope in &observes {
            assert_eq!(envelope["payload"]["session_id"], "ka-1");
            assert_eq!(envelope["payload"]["event"]["stream_state"], "idle");
        }
        assert_eq!(envelopes.last().unwrap()["op"], "end");
    }

    #[tokio::test]
    async fn the_pump_stops_when_the_far_side_goes_away() {
        // A closed peer (the editor exiting mid-turn) must end the copy, not
        // spin on a writer that can never succeed again.
        struct Closed;
        impl AsyncWrite for Closed {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut TaskContext<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
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
        let (tee, mut lines) = mpsc::channel(4);
        // Returns rather than hanging, and tees nothing it could not forward.
        pump(
            &b"{\"type\":\"result\"}\n"[..],
            Closed,
            Direction::FromClaude,
            tee,
            None,
        )
        .await;
        assert!(lines.try_recv().is_err());
    }

    #[tokio::test]
    async fn pump_resyncs_the_title_from_the_watch_channel_alone_with_no_further_reads() {
        // Isolates pump()'s select! loop from the rest of the observe()
        // pipeline: drives the watch channel directly, with a reader that
        // never offers a second chunk, to prove the resync path does not
        // depend on Claude sending anything else.
        let (tee, _lines) = mpsc::channel(4);
        let (title_tx, title_rx) = watch::channel(None);
        let sink = Sink::default();
        let (mut writer_end, reader_end) = tokio::io::duplex(1024);
        writer_end.write_all(b"\x1b]0;2.1.132\x07").await.unwrap();

        let pump_task = tokio::spawn(pump(
            reader_end,
            sink.clone(),
            Direction::FromClaude,
            tee,
            Some(title_rx),
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(sink.contents(), "\x1b]0;2.1.132\x07");

        title_tx.send(Some(PREFIX.to_string())).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            sink.contents().contains("\x1b]0;🟡 Opus · 2.1.132\x07"),
            "expected a resynced title, got: {:?}",
            sink.contents()
        );

        drop(writer_end);
        let _ = pump_task.await;
    }

    #[test]
    fn a_full_tee_drops_lines_rather_than_blocking() {
        let (tee, _lines) = mpsc::channel(1);
        let mut buffer = Vec::new();
        tee_chunk(b"a\nb\nc\n", Direction::ToClaude, &tee, &mut buffer);
        // Two of the three were dropped; nothing blocked, nothing panicked.
        assert_eq!(tee.capacity(), 0);
    }

    const PREFIX: &str = "🟡 Opus · ";

    #[test]
    fn bel_terminated_title_is_rewritten_with_the_model_prefix() {
        let mut rewriter = TitleRewriter::default();
        let out = rewriter.rewrite(b"\x1b]0;2.1.132\x07", Some(PREFIX));
        assert_eq!(out, b"\x1b]0;\xf0\x9f\x9f\xa1 Opus \xc2\xb7 2.1.132\x07");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]0;🟡 Opus · 2.1.132\x07"
        );
    }

    #[test]
    fn st_terminated_title_is_rewritten_with_the_model_prefix() {
        let mut rewriter = TitleRewriter::default();
        let out = rewriter.rewrite(b"\x1b]2;2.1.132\x1b\\", Some(PREFIX));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]2;🟡 Opus · 2.1.132\x1b\\"
        );
    }

    #[test]
    fn no_model_known_yet_leaves_the_title_unchanged() {
        let mut rewriter = TitleRewriter::default();
        let chunk = b"\x1b]0;2.1.132\x07";
        let out = rewriter.rewrite(chunk, None);
        assert_eq!(out, chunk);
    }

    #[test]
    fn a_non_osc_chunk_is_untouched() {
        let mut rewriter = TitleRewriter::default();
        let chunk = b"just some ordinary assistant output, no escapes here\n";
        let out = rewriter.rewrite(chunk, Some(PREFIX));
        assert_eq!(out, chunk);
    }

    #[test]
    fn a_sequence_split_across_two_reads_is_still_rewritten() {
        let mut rewriter = TitleRewriter::default();
        let mut out = rewriter.rewrite(b"before \x1b]0;2.1", Some(PREFIX));
        out.extend(rewriter.rewrite(b".132\x07 after", Some(PREFIX)));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "before \x1b]0;🟡 Opus · 2.1.132\x07 after"
        );
    }

    #[test]
    fn an_osc_code_other_than_0_or_2_is_left_untouched() {
        // OSC 8 (hyperlink) must never be mistaken for a title sequence.
        let mut rewriter = TitleRewriter::default();
        let chunk = b"\x1b]8;;https://example.invalid\x07link text\x1b]8;;\x07";
        let out = rewriter.rewrite(chunk, Some(PREFIX));
        assert_eq!(out, chunk);
    }

    #[test]
    fn an_overlong_sequence_without_a_terminator_is_flushed_unchanged() {
        let mut rewriter = TitleRewriter::default();
        let mut chunk = b"\x1b]0;".to_vec();
        chunk.extend(std::iter::repeat_n(b'x', MAX_OSC_SEQUENCE_BYTES + 16));
        let out = rewriter.rewrite(&chunk, Some(PREFIX));
        assert_eq!(out, chunk);
    }

    #[test]
    fn a_malformed_st_terminator_is_flushed_unchanged() {
        // ESC not immediately followed by `\` is not a valid ST: the whole
        // candidate must be forwarded verbatim rather than guessed at.
        let mut rewriter = TitleRewriter::default();
        let chunk = b"\x1b]0;oops\x1bnope";
        let out = rewriter.rewrite(chunk, Some(PREFIX));
        assert_eq!(out, chunk);
    }

    #[test]
    fn resync_returns_none_before_anything_has_completed() {
        let rewriter = TitleRewriter::default();
        assert_eq!(rewriter.resync(Some(PREFIX)), None);
    }

    #[test]
    fn resync_returns_none_without_a_prefix() {
        let mut rewriter = TitleRewriter::default();
        rewriter.rewrite(b"\x1b]0;2.1.132\x07", None);
        assert_eq!(rewriter.resync(None), None);
    }

    #[test]
    fn resync_reformats_a_title_that_was_forwarded_before_the_model_was_known() {
        // The exact real-world case this exists for: Claude's one-and-only
        // title assertion raced ahead of the model becoming known, so it
        // went out unrewritten — resync must still be able to correct it
        // later, from the cache, with no further help from Claude.
        let mut rewriter = TitleRewriter::default();
        let forwarded = rewriter.rewrite(b"\x1b]0;2.1.132\x07", None);
        assert_eq!(forwarded, b"\x1b]0;2.1.132\x07");
        let corrected = rewriter.resync(Some(PREFIX)).unwrap();
        assert_eq!(
            String::from_utf8(corrected).unwrap(),
            "\x1b]0;🟡 Opus · 2.1.132\x07"
        );
    }

    #[test]
    fn resync_reformats_under_a_changed_prefix_on_a_mid_session_model_switch() {
        let mut rewriter = TitleRewriter::default();
        rewriter.rewrite(b"\x1b]0;2.1.132\x07", Some(PREFIX));
        let resynced = rewriter.resync(Some("🟢 Sonnet · ")).unwrap();
        assert_eq!(
            String::from_utf8(resynced).unwrap(),
            "\x1b]0;🟢 Sonnet · 2.1.132\x07"
        );
    }

    #[test]
    fn flag_is_truthy_recognises_on_and_off_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert!(flag_is_truthy(value), "{value:?} should be truthy");
        }
        for value in ["0", "false", "no", "off", ""] {
            assert!(!flag_is_truthy(value), "{value:?} should not be truthy");
        }
    }

    #[tokio::test]
    async fn the_wrapper_rewrites_claudes_title_end_to_end() {
        let (_dir, socket, _seen) = fake_daemon();
        // Models the real race, not the happy path: real Claude asserts its
        // title once, at startup, *before* the model can possibly be known
        // (the observer has not even received the init line yet) — and,
        // empirically, does not reassert it later in the session. So the
        // very first assertion here is forwarded unrewritten (correct,
        // fail-open behavior), and the `sleep` that follows leaves the child
        // otherwise idle long enough for the async observer pipeline to
        // classify the model and for the pump's watch-driven resync to
        // correct the title on its own, with no further help from Claude.
        let script = concat!(
            r#"printf '\033]0;2.1.132\007';"#,
            r#"printf '{"type":"system","subtype":"init","session_id":"s-1","model":"claude-opus-4-8"}\n';"#,
            r#"sleep 0.2;"#,
            r#"printf 'plain text after the title\n'"#,
        );
        let (_code, forwarded) = wrap_script(script, Some(socket)).await;
        // The original, un-decorated assertion still reaches stdout exactly
        // as Claude sent it — fail-open means "forward unchanged", not "hold
        // back and hope".
        assert!(
            forwarded.contains("\x1b]0;2.1.132\x07"),
            "the original title should still have been forwarded verbatim: {forwarded:?}"
        );
        // And it is followed, with no further title assertion from Claude,
        // by a corrected one carrying the classified model prefix.
        assert!(
            forwarded.contains("\x1b]0;🟡 Opus · 2.1.132\x07"),
            "the title should have been corrected once the model became known: {forwarded:?}"
        );
        let uncorrected_at = forwarded.find("\x1b]0;2.1.132\x07").unwrap();
        let corrected_at = forwarded.find("\x1b]0;🟡 Opus · 2.1.132\x07").unwrap();
        assert!(uncorrected_at < corrected_at);
        assert!(forwarded.ends_with("plain text after the title\n"));
    }

    // Note: there is deliberately no end-to-end test that sets
    // `NO_TITLE_REWRITE_ENV` and drives `wrap_script` — mutating that real
    // variable would race any other test in this module concurrently
    // spawning `wrap_io` (which reads it too), exactly the hazard
    // `request_log.rs`'s env-var tests avoid by mutating a private test-only
    // name instead. `flag_is_truthy_recognises_on_and_off_values` above
    // covers the parsing directly; the one-line `.then_some(title_rx)` wiring
    // in `wrap_io` needs no separate test.
}
