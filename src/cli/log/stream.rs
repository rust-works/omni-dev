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
        if let Ok(file) = File::open(path) {
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
        }
        std::thread::sleep(FOLLOW_POLL);
    }
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
}
