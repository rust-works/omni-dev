//! A running tally of the `gh` records appended to the request log since a
//! fixed instant (#2132).
//!
//! [`aggregate`](super::aggregate) answers "what does this log say for this
//! window" by reading every line, so its cost grows with the whole file — and
//! the log is unbounded unless rotation or `prune` is opted into. The daemon
//! asks one fixed question instead, "what has been logged since I started", of
//! a log that is append-only. [`IncrementalCounts`] answers it by remembering
//! where it stopped reading, so each refresh costs the records appended since
//! the previous one, not the size of the file.
//!
//! The log can also stop being append-only: `omni-dev log prune` renames a
//! rewritten file over it, size-capped rotation renames it away, and a user can
//! truncate it. A cursor that outlives such a change would read from the wrong
//! place and produce a silently wrong tally, so the cursor is trusted only while
//! the file is demonstrably the one it was taken on (see [`Cursor::is_intact`]).
//! Otherwise the tally is dropped and rebuilt by a windowed scan from the first
//! byte, which is what [`aggregate`](super::aggregate) would report.

use std::fs::{File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{Filter, GhCounts};
use crate::request_log::LogRecord;

/// How many bytes ending at the cursor are kept to recognise a file that was
/// rewritten in place. Log lines carry unique ids, so a rewritten file does not
/// reproduce them at the same offset.
const TAIL_LEN: usize = 64;

/// Block size of the backward search for the last complete line at the baseline.
const BACKSCAN_BLOCK: usize = 8 * 1024;

/// Read buffer for the forward scan.
const READ_BUFFER: usize = 64 * 1024;

/// Identity of a file independent of its path: `(device, inode)` on unix. Other
/// platforms have none, and [`Cursor::is_intact`]'s length and tail checks carry
/// the whole burden there.
type FileId = Option<(u64, u64)>;

#[cfg(unix)]
fn file_id(meta: &Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_id(_meta: &Metadata) -> FileId {
    None
}

/// Where the tally has read up to in one particular log file.
#[derive(Debug)]
struct Cursor {
    /// The file this position belongs to.
    id: FileId,
    /// Byte offset just past the last complete line consumed.
    offset: u64,
    /// Up to [`TAIL_LEN`] bytes ending at `offset`, i.e. the end of the last line
    /// consumed.
    tail: Vec<u8>,
}

impl Cursor {
    /// A cursor at the first byte of the file identified by `id`.
    fn start_of(id: FileId) -> Self {
        Self {
            id,
            offset: 0,
            tail: Vec::new(),
        }
    }

    /// Whether `file` — `len` bytes long, identified by `id` — is still the file
    /// this cursor was taken on, with everything before `offset` untouched.
    ///
    /// Three checks, because each misses a case the others catch: a different
    /// file id (rotation, `prune`'s rename), a file shorter than the offset
    /// (truncation), and different bytes where the last consumed line ended (a
    /// file rewritten in place and regrown past the offset, or an inode reused
    /// after its file was deleted).
    fn is_intact(&self, file: &mut File, id: FileId, len: u64) -> io::Result<bool> {
        if self.id != id || len < self.offset {
            return Ok(false);
        }
        let mut seen = vec![0; self.tail.len()];
        file.seek(SeekFrom::Start(self.offset - self.tail.len() as u64))?;
        match file.read_exact(&mut seen) {
            Ok(()) => Ok(seen == self.tail),
            // The file shrank between the length check and the read.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Records that `line` — complete, newline-terminated — was consumed.
    fn advance(&mut self, line: &[u8]) {
        self.offset += line.len() as u64;
        if line.len() >= TAIL_LEN {
            self.tail.clear();
            self.tail.extend_from_slice(&line[line.len() - TAIL_LEN..]);
        } else {
            self.tail.extend_from_slice(line);
            let excess = self.tail.len().saturating_sub(TAIL_LEN);
            self.tail.drain(..excess);
        }
    }

    /// A cursor at the end of the last complete line of the log at `path`, or
    /// `None` when there is no log to position in.
    ///
    /// Reads at most the final line and [`TAIL_LEN`] bytes, never the records
    /// before it. The end of the *last complete line* rather than of the file: a
    /// concurrent writer may be part-way through a line, and a cursor inside it
    /// would start the next read mid-record.
    fn baseline(path: &Path) -> Option<Self> {
        match Self::try_baseline(path) {
            Ok(cursor) => cursor,
            Err(e) => {
                tracing::debug!("github counters: cannot baseline {}: {e}", path.display());
                None
            }
        }
    }

    fn try_baseline(path: &Path) -> io::Result<Option<Self>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        let offset = end_of_last_line(&mut file, meta.len())?;
        let tail = read_tail(&mut file, offset)?;
        Ok(Some(Self {
            id: file_id(&meta),
            offset,
            tail,
        }))
    }
}

/// The offset just past the last `\n` in the first `len` bytes of `file`, or 0
/// when there is none. Scans backwards, so it reads only the final line.
fn end_of_last_line(file: &mut File, len: u64) -> io::Result<u64> {
    let mut block = [0u8; BACKSCAN_BLOCK];
    let mut end = len;
    while end > 0 {
        let start = end.saturating_sub(BACKSCAN_BLOCK as u64);
        let want = usize::try_from(end - start).unwrap_or(BACKSCAN_BLOCK);
        let chunk = &mut block[..want];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(i) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Up to [`TAIL_LEN`] bytes of `file` ending at `offset`.
fn read_tail(file: &mut File, offset: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(offset).map_or(TAIL_LEN, |o| o.min(TAIL_LEN));
    let mut tail = vec![0; len];
    file.seek(SeekFrom::Start(offset - len as u64))?;
    file.read_exact(&mut tail)?;
    Ok(tail)
}

/// A running tally of the `gh` records logged since a fixed instant, refreshed
/// by reading only what was appended since the previous refresh.
///
/// Equivalent to [`aggregate`](super::aggregate) over `[since, ∞)`, minus the
/// cost: construction reads no records, and each [`refresh`](Self::refresh)
/// reads only the complete lines appended after the last one. A line still being
/// written (no trailing newline yet) is left for the next refresh, so a record is
/// counted once, whole. That is the one way the two differ over the same bytes:
/// `aggregate` counts a final line with no newline if it happens to parse, this
/// waits for the newline.
#[derive(Debug)]
pub struct IncrementalCounts {
    path: PathBuf,
    since: DateTime<Utc>,
    filter: Filter,
    counts: GhCounts,
    /// `None` until a log file has been seen.
    cursor: Option<Cursor>,
}

impl IncrementalCounts {
    /// Starts tallying the log at `path` from now: records already in it are
    /// never read, and the tally begins at zero.
    ///
    /// The log's extent is taken *before* the clock is read. A record stamped at
    /// or after [`since`](Self::since) was therefore appended after the baseline,
    /// so the bytes the next refresh reads contain every record the window
    /// admits. Reading them in the other order could skip a record stamped just
    /// after the clock read but appended before the baseline.
    pub fn start(path: PathBuf) -> Self {
        let cursor = Cursor::baseline(&path);
        Self::new(path, Utc::now(), cursor)
    }

    /// [`start`](Self::start) with the window opening at `since` instead of now,
    /// so tests can stamp their records deterministically.
    #[cfg(test)]
    fn start_since(path: PathBuf, since: DateTime<Utc>) -> Self {
        let cursor = Cursor::baseline(&path);
        Self::new(path, since, cursor)
    }

    fn new(path: PathBuf, since: DateTime<Utc>, cursor: Option<Cursor>) -> Self {
        Self {
            path,
            since,
            filter: Filter {
                since: Some(since),
                until: None,
                source: None,
            },
            counts: GhCounts::default(),
            cursor,
        }
    }

    /// The instant the window opened: records stamped before it are not counted.
    pub fn since(&self) -> DateTime<Utc> {
        self.since
    }

    /// The tally as of the last [`refresh`](Self::refresh).
    pub fn counts(&self) -> &GhCounts {
        &self.counts
    }

    /// Reads the records appended since the last call into the tally.
    ///
    /// `stop` is checked before every line; once it returns `true` the refresh
    /// ends there, leaving the tally and cursor consistent at that line, so the
    /// next refresh resumes without losing or repeating anything. It is how a
    /// caller on a blocking thread — which cannot be cancelled from outside —
    /// gives up on a long read, e.g. the rebuild after the log was replaced.
    ///
    /// Returns `true` when the tally is current as of the last complete line, and
    /// `false` when it may be behind: `stop` fired, or the log could not be read
    /// (logged at debug; best-effort like the rest of the log stack).
    pub fn refresh(&mut self, mut stop: impl FnMut() -> bool) -> bool {
        match self.try_refresh(&mut stop) {
            Ok(complete) => complete,
            Err(e) => {
                tracing::debug!(
                    "github counters: reading {} failed: {e}",
                    self.path.display()
                );
                false
            }
        }
    }

    fn try_refresh(&mut self, stop: &mut impl FnMut() -> bool) -> io::Result<bool> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // No log, so nothing in it: `aggregate`'s answer for a missing
                // file. A log created later is scanned from its first byte.
                self.counts = GhCounts::default();
                self.cursor = None;
                return Ok(true);
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        let mut cursor = self.resume(&mut file, &meta);
        let result = self.read_new_lines(&mut file, &mut cursor, stop);
        // Keep the cursor whatever happened: it is consistent at the last line
        // consumed, and a failed read should not cost a rebuild next time.
        self.cursor = Some(cursor);
        result
    }

    /// The cursor to read from: the stored one when `file` is demonstrably the
    /// file it was taken on, otherwise a fresh one at the file's first byte with
    /// the tally reset.
    fn resume(&mut self, file: &mut File, meta: &Metadata) -> Cursor {
        let id = file_id(meta);
        if let Some(cursor) = self.cursor.take() {
            match cursor.is_intact(file, id, meta.len()) {
                Ok(true) => return cursor,
                Ok(false) => {}
                Err(e) => tracing::debug!(
                    "github counters: cannot verify the position in {}, rescanning: {e}",
                    self.path.display()
                ),
            }
        }
        // First sight of this file, or it was replaced, rotated, truncated or
        // rewritten: what was tallied came from something else.
        self.counts = GhCounts::default();
        Cursor::start_of(id)
    }

    fn read_new_lines(
        &mut self,
        file: &mut File,
        cursor: &mut Cursor,
        stop: &mut impl FnMut() -> bool,
    ) -> io::Result<bool> {
        file.seek(SeekFrom::Start(cursor.offset))?;
        let mut reader = BufReader::with_capacity(READ_BUFFER, file);
        let mut line = Vec::new();
        loop {
            if stop() {
                return Ok(false);
            }
            line.clear();
            reader.read_until(b'\n', &mut line)?;
            if line.last() != Some(&b'\n') {
                // End of file, or a line still being written.
                return Ok(true);
            }
            // A complete line that is not a record (malformed, blank) is skipped
            // and consumed, as `aggregate` skips it.
            if let Ok(rec) = serde_json::from_slice::<LogRecord>(&line) {
                self.filter.tally(&mut self.counts, &rec);
            }
            cursor.advance(&line);
        }
    }

    /// The cursor's offset, for tests that assert what was and was not read.
    #[cfg(test)]
    fn offset(&self) -> Option<u64> {
        self.cursor.as_ref().map(|c| c.offset)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::github_metrics::aggregate;
    use chrono::{Duration, SecondsFormat};
    use std::io::Write;

    fn since() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-21T10:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// An RFC3339 stamp `secs` seconds after [`since`] (negative: before it).
    fn stamp(secs: i64) -> String {
        (since() + Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    /// A `gh` record line, newline-terminated.
    fn gh(secs: i64, command: &[&str], source: &str) -> String {
        format!(
            "{{\"kind\":\"gh\",\"timestamp\":\"{}\",\"command\":{},\"source\":\"{source}\"}}\n",
            stamp(secs),
            serde_json::to_string(command).unwrap(),
        )
    }

    /// A log in a temp dir, addressed by path so tests can replace the file.
    struct Log {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Log {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("log.jsonl");
            Self { _dir: dir, path }
        }

        /// Replaces the contents in place (same inode, truncated first).
        fn write(&self, text: &str) {
            std::fs::write(&self.path, text).unwrap();
        }

        fn append(&self, text: &str) {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        }

        /// Replaces the file the way `prune` does: a new file renamed over it.
        fn replace(&self, text: &str) {
            let tmp = self.path.with_extension("tmp");
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, &self.path).unwrap();
        }

        fn len(&self) -> u64 {
            std::fs::metadata(&self.path).unwrap().len()
        }

        fn full_scan(&self) -> GhCounts {
            aggregate(&self.path, Some(since()), None, None)
        }

        fn start(&self) -> IncrementalCounts {
            IncrementalCounts::start_since(self.path.clone(), since())
        }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn records_before_the_baseline_are_never_read() {
        let log = Log::new();
        // Valid `gh` records inside the window: a full scan counts all of them, so
        // the incremental tally can only be empty by not having read them.
        let pre: String = (0..5).map(|n| gh(10 + n, &["pr", "list"], "cli")).collect();
        log.write(&pre);
        assert_eq!(log.full_scan().total(), 5);

        let mut tally = log.start();
        assert!(tally.refresh(never));

        assert_eq!(tally.counts().total(), 0);
        assert_eq!(tally.offset(), Some(log.len()));
    }

    #[test]
    fn appended_records_match_a_full_scan() {
        let log = Log::new();
        log.write(&gh(-100, &["pr", "list"], "cli")); // pre-boot
        let mut tally = log.start();

        log.append(&gh(1, &["api", "graphql"], "daemon"));
        log.append(&gh(2, &["pr", "list"], "cli"));
        log.append(&gh(3, &["--version"], "cli"));
        log.append("{\"kind\":\"http\",\"timestamp\":\"2026-07-21T10:00:04.000Z\"}\n");
        log.append("not json\n\n");
        log.append(&gh(-5, &["pr", "list"], "cli")); // stamped before the window
        log.append("{\"kind\":\"gh\",\"timestamp\":\"bad\",\"command\":[\"pr\"]}\n");
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 3);
        assert_eq!(*tally.counts(), log.full_scan());

        log.append(&gh(6, &["api", "graphql"], "mcp"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 4);
        assert_eq!(*tally.counts(), log.full_scan());

        // Nothing appended: nothing recounted.
        assert!(tally.refresh(never));
        assert_eq!(*tally.counts(), log.full_scan());
        assert_eq!(tally.offset(), Some(log.len()));
    }

    #[test]
    fn a_partial_trailing_line_waits_for_its_newline() {
        let log = Log::new();
        let mut tally = log.start();

        let line = gh(1, &["pr", "list"], "cli");
        let (head, rest) = line.split_at(line.len() / 2);
        log.append(head);
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 0);
        assert_eq!(tally.offset(), Some(0));

        log.append(rest);
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 1);
        assert_eq!(*tally.counts(), log.full_scan());

        // A line that is complete JSON but not yet newline-terminated is still
        // being written: it is counted once, when its newline arrives.
        let line = gh(2, &["pr", "view"], "cli");
        log.append(line.trim_end());
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 1);
        log.append("\n");
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 2);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn a_baseline_inside_a_line_starts_at_that_line() {
        let log = Log::new();
        let first = gh(-1, &["pr", "list"], "cli");
        let second = gh(2, &["pr", "view"], "cli");
        let (head, rest) = second.split_at(second.len() / 2);
        log.write(&format!("{first}{head}"));

        let mut tally = log.start();
        assert_eq!(tally.offset(), Some(first.len() as u64));

        // The record being written at the baseline is stamped inside the window
        // and appended whole, so it must be counted; the one before it is not.
        log.append(rest);
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 1);
        assert_eq!(tally.counts().by_subcommand["pr view"], 1);
    }

    #[test]
    fn the_last_line_is_found_across_search_blocks() {
        let log = Log::new();
        let first = gh(1, &["pr", "list"], "cli");
        let long = "x".repeat(3 * BACKSCAN_BLOCK + 17);
        log.write(&format!("{first}{long}"));

        let mut file = File::open(&log.path).unwrap();
        assert_eq!(
            end_of_last_line(&mut file, log.len()).unwrap(),
            first.len() as u64
        );
        assert_eq!(end_of_last_line(&mut file, 0).unwrap(), 0);

        log.write(&long); // no newline anywhere
        let mut file = File::open(&log.path).unwrap();
        assert_eq!(end_of_last_line(&mut file, log.len()).unwrap(), 0);
    }

    #[test]
    fn a_replaced_log_is_rescanned_from_its_first_byte() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        log.append(&gh(2, &["pr", "list"], "cli"));
        log.append(&gh(3, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 3);

        // What `prune` does: a rewritten file renamed over the log. It is *longer*
        // than the stored offset, so only the file id can tell.
        log.replace(&format!(
            "{}{}{}",
            gh(4, &["api", "graphql"], "daemon"),
            gh(5, &["api", "graphql"], "daemon"),
            gh(-9, &["pr", "list"], "cli"),
        ));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 2);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn a_rotated_log_is_followed_to_its_replacement() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));

        // Size-capped rotation: the log is renamed away and a fresh one started.
        std::fs::rename(&log.path, log.path.with_extension("jsonl.1")).unwrap();
        log.append(&gh(2, &["api", "graphql"], "daemon"));
        assert!(tally.refresh(never));
        assert_eq!(*tally.counts(), log.full_scan());
        assert_eq!(tally.counts().total(), 1);

        // Rotated away and not yet recreated: no log, no counts.
        std::fs::remove_file(&log.path).unwrap();
        assert!(tally.refresh(never));
        assert_eq!(*tally.counts(), GhCounts::default());
        assert_eq!(tally.offset(), None);

        log.append(&gh(3, &["pr", "view"], "cli"));
        log.append(&gh(4, &["pr", "view"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 2);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn a_log_truncated_below_the_offset_is_rescanned() {
        let log = Log::new();
        let mut tally = log.start();
        for n in 1..=4 {
            log.append(&gh(n, &["pr", "list"], "cli"));
        }
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 4);

        log.write(&gh(9, &["api", "graphql"], "daemon")); // same inode, shorter
        assert!(log.len() < tally.offset().unwrap());
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 1);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn a_log_rewritten_in_place_and_regrown_past_the_offset_is_rescanned() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        log.append(&gh(2, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));
        let stored = tally.offset().unwrap();

        // Same inode, and by the time it is read again it is longer than the
        // stored offset, so neither the file id nor the length notices. Only the
        // bytes before the offset have changed.
        let rewritten: String = (10..16)
            .map(|n| gh(n, &["api", "graphql"], "daemon"))
            .collect();
        log.write(&rewritten);
        assert!(log.len() > stored);
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 6);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn a_log_created_after_the_baseline_is_scanned_from_its_first_byte() {
        let log = Log::new();
        let mut tally = log.start();
        assert_eq!(tally.offset(), None);
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 0);

        log.append(&gh(-10, &["pr", "list"], "cli"));
        log.append(&gh(1, &["pr", "list"], "cli"));
        log.append(&gh(2, &["pr", "view"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 2);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn a_stopped_refresh_resumes_where_it_stopped() {
        let log = Log::new();
        let mut tally = log.start();
        for n in 1..=5 {
            log.append(&gh(n, &["pr", "list"], "cli"));
        }

        // Stop is consulted before each line: let two through, then refuse.
        let mut calls = 0;
        let complete = tally.refresh(|| {
            calls += 1;
            calls > 2
        });
        assert!(!complete);
        assert_eq!(tally.counts().total(), 2);

        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 5);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn a_stopped_rebuild_keeps_what_it_read_and_continues() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));

        // The replacement forces a rebuild; stop it part-way, then let it finish.
        let replacement: String = (10..16).map(|n| gh(n, &["pr", "view"], "cli")).collect();
        log.replace(&replacement);
        let mut calls = 0;
        assert!(!tally.refresh(|| {
            calls += 1;
            calls > 3
        }));
        assert_eq!(tally.counts().total(), 3);

        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 6);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[test]
    fn the_tail_keeps_the_last_bytes_across_short_and_long_lines() {
        let mut cursor = Cursor::start_of(None);
        cursor.advance(b"ab\n");
        cursor.advance(b"cd\n");
        assert_eq!(cursor.tail, b"ab\ncd\n");
        assert_eq!(cursor.offset, 6);

        let long: Vec<u8> = (0..200u8).collect();
        cursor.advance(&long);
        assert_eq!(cursor.tail, &long[long.len() - TAIL_LEN..]);

        cursor.advance(b"z\n");
        assert_eq!(cursor.tail.len(), TAIL_LEN);
        assert!(cursor.tail.ends_with(b"z\n"));
        assert_eq!(cursor.offset, 6 + 200 + 2);
    }
}
