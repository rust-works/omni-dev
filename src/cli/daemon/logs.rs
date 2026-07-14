//! `omni-dev daemon logs` — read (and optionally follow) the daemon's log file.
//!
//! The daemon's stdout/stderr is sunk to a `0600` `daemon.log` beside the control
//! socket (#1316): the launchd LaunchAgent points `StandardOutPath`/`StandardErrorPath`
//! there on macOS, and the off-macOS detached-spawn launcher appends there. This
//! command reads that plaintext file: it prints a trailing window of lines and,
//! with `--follow`, tails newly appended lines. It never connects to the daemon,
//! so it works whether or not one is currently running. (Under a systemd user
//! unit the daemon logs to the journal instead — `journalctl --user`; no
//! `daemon.log` exists on that path.)
//!
//! The file is plaintext, so the NDJSON `omni-dev log` renderer can't be reused;
//! this mirrors that command's tail scaffolding (a ring buffer for the backlog,
//! a seek-from-offset poll for follow, broken-pipe-as-clean-exit) over raw lines.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;

use crate::daemon::{paths, server};

/// Poll interval while following the log.
const FOLLOW_POLL: Duration = Duration::from_millis(250);

/// Default number of trailing lines shown when `--lines` is not given.
const DEFAULT_LINES: usize = 200;

/// Reads (and optionally follows) the daemon's log file.
#[derive(Parser)]
pub struct LogsCommand {
    /// Control-socket path; the log sits beside it (`<socket dir>/daemon.log`).
    /// Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Show at most this many trailing lines (`0` shows the whole file).
    #[arg(short = 'n', long, default_value_t = DEFAULT_LINES)]
    pub lines: usize,
    /// Follow the log, printing new lines as they are appended (Ctrl-C to stop).
    #[arg(short = 'f', long)]
    pub follow: bool,
}

impl LogsCommand {
    /// Executes the logs command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let path = paths::log_path_for_socket(&socket);
        let limit = (self.lines != 0).then_some(self.lines);
        let follow = self.follow;
        // Blocking file I/O plus a sleep loop when following: run it off the
        // async reactor so it never stalls the runtime.
        tokio::task::spawn_blocking(move || {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            tail(&path, limit, follow, &mut out)
        })
        .await
        .map_err(|e| anyhow!("daemon logs reader panicked: {e}"))?
    }
}

/// Prints the trailing window of `path` to `out`, then follows it when `follow`
/// is set. Taking the writer (rather than locking stdout internally) lets the
/// non-follow backlog path be exercised in tests against an in-memory buffer.
fn tail<W: Write>(path: &Path, limit: Option<usize>, follow: bool, out: &mut W) -> Result<()> {
    let mut pos = 0u64;
    match File::open(path) {
        Ok(file) => {
            let mut reader = BufReader::new(file);
            pos = print_backlog(&mut reader, limit, out)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if !follow {
                eprintln!(
                    "No daemon log yet at {} (start the daemon to create it).",
                    path.display()
                );
                return Ok(());
            }
            eprintln!("Waiting for daemon log at {}…", path.display());
        }
        Err(e) => return Err(e.into()),
    }

    if follow {
        if let Err(e) = follow_loop(path, pos, out) {
            return swallow_broken_pipe(e);
        }
    }
    Ok(())
}

/// Reads every existing line; with `limit`, keeps only the most recent N in a
/// ring buffer and prints them at the end. Returns the end-of-file byte offset.
fn print_backlog<R: BufRead, W: Write>(
    reader: &mut R,
    limit: Option<usize>,
    out: &mut W,
) -> Result<u64> {
    let mut pos = 0u64;
    let mut ring: VecDeque<String> = VecDeque::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        pos += n as u64;
        match limit {
            Some(cap) => {
                ring.push_back(line.clone());
                while ring.len() > cap {
                    ring.pop_front();
                }
            }
            // `read_line` keeps the trailing newline, so `write!` (not `writeln!`).
            None => write!(out, "{line}").or_else(ignore_broken_pipe)?,
        }
    }
    for buffered in &ring {
        write!(out, "{buffered}").or_else(ignore_broken_pipe)?;
    }
    Ok(pos)
}

/// Tails the file from `pos`, printing newly appended complete lines forever
/// (until interrupted). Restarts from the top on truncation/rotation.
fn follow_loop<W: Write>(path: &Path, mut pos: u64, out: &mut W) -> Result<()> {
    loop {
        pos = drain_appended(path, pos, out)?;
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Emits any complete lines appended past `pos`, returning the new position.
/// Restarts from the top if the file shrank (truncation/rotation); a no-op if
/// the file is absent or has not grown. A trailing partial line is left for the
/// next call.
fn drain_appended<W: Write>(path: &Path, mut pos: u64, out: &mut W) -> Result<u64> {
    let Ok(file) = File::open(path) else {
        return Ok(pos);
    };
    let len = file.metadata().map_or(pos, |m| m.len());
    if len < pos {
        pos = 0; // truncated or rotated — restart
    }
    if len > pos {
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(pos))?;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 || !line.ends_with('\n') {
                break; // EOF or partial trailing line — wait for more
            }
            pos += n as u64;
            write!(out, "{line}")?;
            out.flush()?;
        }
    }
    Ok(pos)
}

/// Maps a broken-pipe write error (e.g. piping into `head`) to a clean exit.
fn ignore_broken_pipe(e: io::Error) -> io::Result<()> {
    if e.kind() == io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(e)
    }
}

/// Maps a broken-pipe error (downcast from `anyhow`) to a clean exit.
fn swallow_broken_pipe(e: anyhow::Error) -> Result<()> {
    if let Some(io_err) = e.downcast_ref::<io::Error>() {
        if io_err.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
    }
    Err(e)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::io::Cursor;

    fn sample() -> String {
        (0..5).fold(String::new(), |mut acc, i| {
            let _ = writeln!(acc, "line {i}");
            acc
        })
    }

    #[test]
    fn backlog_emits_all_without_limit() {
        let mut reader = BufReader::new(Cursor::new(sample()));
        let mut out = Vec::new();
        let pos = print_backlog(&mut reader, None, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 5);
        assert!(text.starts_with("line 0\n"));
        assert!(text.ends_with("line 4\n"));
        assert_eq!(pos, sample().len() as u64);
    }

    #[test]
    fn backlog_limit_keeps_the_most_recent_lines() {
        let mut reader = BufReader::new(Cursor::new(sample()));
        let mut out = Vec::new();
        print_backlog(&mut reader, Some(2), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "line 3\nline 4\n");
    }

    #[test]
    fn drain_appended_reads_only_new_complete_lines_and_handles_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, sample()).unwrap();

        // First drain reads the whole backlog and advances the position.
        let mut out = Vec::new();
        let pos = drain_appended(&path, 0, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 5);
        assert!(pos > 0);

        // Appending a line and draining from `pos` yields only that line.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "line 5").unwrap();
        }
        let mut out = Vec::new();
        let pos2 = drain_appended(&path, pos, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "line 5\n");
        assert!(pos2 > pos);

        // A trailing partial line (no newline) is held back until it completes.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            write!(f, "partial").unwrap();
        }
        let mut out = Vec::new();
        let pos3 = drain_appended(&path, pos2, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().is_empty());
        assert_eq!(pos3, pos2, "a partial line must not advance the position");

        // Truncation (rotation) resets to the top and re-reads.
        std::fs::write(&path, "fresh\n").unwrap();
        let mut out = Vec::new();
        drain_appended(&path, pos2, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "fresh\n");

        // A missing file is a no-op that preserves the position.
        let missing = dir.path().join("gone.log");
        let mut out = Vec::new();
        assert_eq!(drain_appended(&missing, 7, &mut out).unwrap(), 7);
        assert!(String::from_utf8(out).unwrap().is_empty());
    }

    #[test]
    fn broken_pipe_is_swallowed_other_errors_propagate() {
        use std::io::{Error, ErrorKind};
        assert!(ignore_broken_pipe(Error::from(ErrorKind::BrokenPipe)).is_ok());
        assert!(ignore_broken_pipe(Error::from(ErrorKind::PermissionDenied)).is_err());
        assert!(swallow_broken_pipe(anyhow!(Error::from(ErrorKind::BrokenPipe))).is_ok());
        assert!(swallow_broken_pipe(anyhow!("unrelated")).is_err());
    }

    #[tokio::test]
    async fn execute_reads_the_log_beside_the_socket() {
        // `execute` resolves `daemon.log` beside the socket and reads it via
        // `spawn_blocking`; a non-follow run over an existing file returns Ok.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("daemon.sock");
        std::fs::write(dir.path().join("daemon.log"), "hello\n").unwrap();
        let cmd = LogsCommand {
            socket: Some(sock),
            lines: 200,
            follow: false,
        };
        cmd.execute().await.unwrap();
    }

    #[test]
    fn tail_on_missing_file_without_follow_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("daemon.log");
        let mut out = Vec::new();
        // A missing file (not following) is a clean no-op: no output, no error.
        assert!(tail(&missing, Some(10), false, &mut out).is_ok());
        assert!(out.is_empty());
    }

    #[test]
    fn tail_prints_the_backlog_window_of_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, sample()).unwrap();

        // No limit → the whole file.
        let mut out = Vec::new();
        tail(&path, None, false, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), sample());

        // A limit → only the trailing window.
        let mut out = Vec::new();
        tail(&path, Some(2), false, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "line 3\nline 4\n");
    }
}
