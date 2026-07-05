//! Streaming reader: filter and render the log line by line.
//!
//! The backlog is read without buffering the whole file (a `--limit` keeps only
//! the most recent N matches in a ring buffer); `--follow` then tails newly
//! appended complete lines. A broken pipe (e.g. piping into `head`) is treated
//! as a clean exit, not an error.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use super::format;
use super::query::Filter;
use super::Format;
use crate::request_log::LogRecord;

/// Poll interval while following the log.
const FOLLOW_POLL: Duration = Duration::from_millis(250);

/// Runs a one-shot backlog scan (no follow), capturing the rendered matches
/// into a single string for programmatic consumers (the MCP `log_search` tool)
/// rather than writing to stdout. A missing log file yields an empty string.
///
/// Only compiled with the `mcp` feature — its sole caller is the MCP
/// `log_search` capture path.
#[cfg(feature = "mcp")]
pub(crate) fn run_capture(
    path: &Path,
    filter: &Filter,
    format: Format,
    limit: Option<usize>,
) -> Result<String> {
    use anyhow::Context as _;

    let mut out: Vec<u8> = Vec::new();
    match File::open(path) {
        Ok(file) => {
            let mut reader = BufReader::new(file);
            emit_backlog(&mut reader, filter, format, limit, &mut out)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    String::from_utf8(out).context("log output was not valid UTF-8")
}

/// Streams the log file at `path`, applying `filter` and rendering as `format`.
pub fn run(
    path: &Path,
    filter: &Filter,
    format: Format,
    limit: Option<usize>,
    follow: bool,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut pos = 0u64;
    match File::open(path) {
        Ok(file) => {
            let mut reader = BufReader::new(file);
            pos = emit_backlog(&mut reader, filter, format, limit, &mut out)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if !follow {
                return Ok(());
            }
        }
        Err(e) => return Err(e.into()),
    }

    if follow {
        if let Err(e) = follow_loop(path, filter, format, pos, &mut out) {
            return swallow_broken_pipe(e);
        }
    }
    Ok(())
}

/// Reads every existing line, emitting matches. With `limit`, only the most
/// recent N matches are kept (ring buffer) and printed at the end; without it,
/// matches stream out as they are read. Returns the byte offset of end-of-file.
fn emit_backlog<R: BufRead, W: Write>(
    reader: &mut R,
    filter: &Filter,
    format: Format,
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
        if let Some(rendered) = render_if_match(&line, filter, format) {
            match limit {
                Some(cap) => {
                    ring.push_back(rendered);
                    while ring.len() > cap {
                        ring.pop_front();
                    }
                }
                None => writeln!(out, "{rendered}").or_else(ignore_broken_pipe)?,
            }
        }
    }
    for rendered in &ring {
        writeln!(out, "{rendered}").or_else(ignore_broken_pipe)?;
    }
    Ok(pos)
}

/// Tails the file from `pos`, printing newly appended complete lines forever
/// (until the process is interrupted). Restarts from the top on truncation.
fn follow_loop<W: Write>(
    path: &Path,
    filter: &Filter,
    format: Format,
    mut pos: u64,
    out: &mut W,
) -> Result<()> {
    loop {
        pos = drain_appended(path, filter, format, pos, out)?;
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Reads and emits any complete lines appended past `pos`, returning the new
/// position. Restarts from the top if the file shrank (truncation/rotation);
/// a no-op (returns `pos` unchanged) if the file is absent or has not grown.
/// A trailing partial line (no newline yet) is left for the next call.
fn drain_appended<W: Write>(
    path: &Path,
    filter: &Filter,
    format: Format,
    mut pos: u64,
    out: &mut W,
) -> Result<u64> {
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
            if let Some(rendered) = render_if_match(&line, filter, format) {
                writeln!(out, "{rendered}")?;
                out.flush()?;
            }
        }
    }
    Ok(pos)
}

/// Parses one raw line, returning its rendering when it matches the filter.
/// Malformed lines (and empties) are silently skipped.
fn render_if_match(line: &str, filter: &Filter, format: Format) -> Option<String> {
    let raw = line.trim_end_matches(['\n', '\r']);
    if raw.is_empty() {
        return None;
    }
    let rec: LogRecord = serde_json::from_str(raw).ok()?;
    if filter.matches(&rec, raw) {
        Some(format::render(&rec, raw, format))
    } else {
        None
    }
}

/// Maps a broken-pipe write error to `Ok(())`; propagates anything else.
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
    use crate::cli::log::query::FilterInput;
    use std::io::Cursor;

    fn empty_filter() -> Filter {
        Filter::build(FilterInput {
            since: None,
            until: None,
            method: None,
            status: None,
            service: None,
            command: None,
            url: None,
            grep: None,
            fuzzy: &[],
            query: &[],
            id: None,
        })
        .unwrap()
    }

    fn sample_lines() -> String {
        let mut s = String::new();
        for i in 0..5 {
            s.push_str(&format!(
                r#"{{"id":"{i}","invocation_id":"inv","kind":"http","timestamp":"2026-06-22T00:00:0{i}.000Z","service":"jira","method":"GET","status_code":200,"url":"/x/{i}"}}"#,
            ));
            s.push('\n');
        }
        s
    }

    #[test]
    fn backlog_emits_all_without_limit() {
        let mut reader = BufReader::new(Cursor::new(sample_lines()));
        let mut out = Vec::new();
        emit_backlog(&mut reader, &empty_filter(), Format::Json, None, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 5);
        // JSON format is byte-identical to the input lines.
        assert!(text.contains(r#""url":"/x/0""#));
        assert!(text.contains(r#""url":"/x/4""#));
    }

    #[test]
    fn backlog_limit_keeps_most_recent() {
        let mut reader = BufReader::new(Cursor::new(sample_lines()));
        let mut out = Vec::new();
        emit_backlog(
            &mut reader,
            &empty_filter(),
            Format::Json,
            Some(2),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains(r#""url":"/x/3""#));
        assert!(text.contains(r#""url":"/x/4""#));
        assert!(!text.contains(r#""url":"/x/0""#));
    }

    #[test]
    fn backlog_skips_malformed_and_empty_lines() {
        let input = "not json\n\n{\"id\":\"1\",\"kind\":\"http\"}\n";
        let mut reader = BufReader::new(Cursor::new(input));
        let mut out = Vec::new();
        emit_backlog(&mut reader, &empty_filter(), Format::Json, None, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains(r#""id":"1""#));
    }

    #[test]
    fn backlog_applies_filter() {
        let filter = Filter::build(FilterInput {
            since: None,
            until: None,
            method: None,
            status: Some("5xx"),
            service: None,
            command: None,
            url: None,
            grep: None,
            fuzzy: &[],
            query: &[],
            id: None,
        })
        .unwrap();
        let mut reader = BufReader::new(Cursor::new(sample_lines()));
        let mut out = Vec::new();
        emit_backlog(&mut reader, &filter, Format::Json, None, &mut out).unwrap();
        // All sample lines are status 200, so nothing matches 5xx.
        assert!(String::from_utf8(out).unwrap().is_empty());
    }

    #[test]
    fn run_on_missing_file_without_follow_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        // No file yet and not following: clean no-op.
        assert!(run(&missing, &empty_filter(), Format::Json, None, false).is_ok());
    }

    #[test]
    fn run_reads_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, sample_lines()).unwrap();
        // Exercises the open + backlog path (output goes to captured stdout).
        assert!(run(&path, &empty_filter(), Format::Json, Some(2), false).is_ok());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn run_capture_returns_matches_as_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, sample_lines()).unwrap();
        let out = run_capture(&path, &empty_filter(), Format::Json, Some(2)).unwrap();
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains(r#""url":"/x/4""#));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn run_capture_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        let out = run_capture(&missing, &empty_filter(), Format::Json, None).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn broken_pipe_is_swallowed_other_errors_propagate() {
        use std::io::{Error, ErrorKind};

        assert!(ignore_broken_pipe(Error::from(ErrorKind::BrokenPipe)).is_ok());
        assert!(ignore_broken_pipe(Error::from(ErrorKind::PermissionDenied)).is_err());

        assert!(
            swallow_broken_pipe(anyhow::Error::from(Error::from(ErrorKind::BrokenPipe))).is_ok()
        );
        assert!(swallow_broken_pipe(anyhow::anyhow!("unrelated")).is_err());
    }

    #[test]
    fn drain_appended_reads_only_new_complete_lines() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, sample_lines()).unwrap();

        // First drain reads the whole backlog and advances the position.
        let mut out = Vec::new();
        let pos = drain_appended(&path, &empty_filter(), Format::Json, 0, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 5);
        assert!(pos > 0);

        // Appending two lines and draining from `pos` yields only those.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, r#"{{"id":"5","kind":"http"}}"#).unwrap();
        writeln!(f, r#"{{"id":"6","kind":"http"}}"#).unwrap();
        let mut out = Vec::new();
        let pos2 = drain_appended(&path, &empty_filter(), Format::Json, pos, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains(r#""id":"5""#));
        assert!(pos2 > pos);

        // A trailing partial line (no newline) is left for the next call.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, r#"{{"id":"7","kind":"http"}}"#).unwrap();
        let mut out = Vec::new();
        let pos3 = drain_appended(&path, &empty_filter(), Format::Json, pos2, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().is_empty());
        assert_eq!(pos3, pos2, "partial line does not advance the position");

        // Truncation resets to the top and re-reads.
        std::fs::write(&path, "{\"id\":\"x\",\"kind\":\"http\"}\n").unwrap();
        let mut out = Vec::new();
        drain_appended(&path, &empty_filter(), Format::Json, pos2, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains(r#""id":"x""#));

        // A missing file is a no-op that preserves the position.
        let missing = dir.path().join("gone.jsonl");
        let mut out = Vec::new();
        assert_eq!(
            drain_appended(&missing, &empty_filter(), Format::Json, 7, &mut out).unwrap(),
            7
        );
        assert!(String::from_utf8(out).unwrap().is_empty());
    }
}
