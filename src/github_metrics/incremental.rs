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
//!
//! Rotation is the one change that loses nothing, and the one the live file
//! alone cannot be told apart from a prune by (#2162). It only *renames*: the file
//! the cursor was taken on is still next to the log as `log.jsonl.N`, under the
//! same file id, with the records appended after the last refresh at the end of
//! it. So the tally follows its position there, reads that tail, then every newer
//! rotated file and the new live file from their first bytes, and keeps its
//! counts. A prune unlinks the file it replaces, so the cursor's file is never
//! found; neither is one rotated out of retention.
//!
//! Anything else — the cursor's file is not found, or is found but no longer
//! matches — drops the tally and rebuilds it by a windowed scan of the live file
//! from the first byte, which is what [`aggregate`](super::aggregate) would
//! report.

use std::fs::{File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{Filter, GhCounts};
use crate::request_log::{self, LogRecord};

/// How many bytes ending at the cursor are kept to recognise a file that was
/// rewritten in place. Enough to tell one from the next in all but a contrived
/// case, which [`Cursor::is_intact`] describes.
const TAIL_LEN: usize = 64;

/// Block size of the backward search for the last complete line at the baseline.
const BACKSCAN_BLOCK: usize = 8 * 1024;

/// How far back from the end of the log the baseline looks for a line boundary.
/// A log's last line is a record, normally a few hundred bytes, so this is far
/// more than needed; it only bounds the pathological case of a huge run of bytes
/// with no newline, so that taking the baseline is cheap whatever the file holds.
const BASELINE_SCAN_LIMIT: u64 = 4 * 1024 * 1024;

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

/// A log file open for reading, with the identity and length it had when it was
/// opened. Holding the handle keeps the file readable through any rename that
/// follows, which is what lets a rotation happen while the chain is being read.
struct Open {
    file: File,
    id: FileId,
    len: u64,
}

impl Open {
    fn at(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        Ok(Self {
            file,
            id: file_id(&meta),
            len: meta.len(),
        })
    }
}

/// What one refresh reads: `files`, oldest first, the first from `cursor`'s
/// offset and each later one from its first byte. The live file, when there is
/// one, is last.
struct Plan {
    cursor: Cursor,
    files: Vec<Open>,
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
    /// file id (rotation, `prune`'s rename — [`IncrementalCounts::locate`] tells
    /// them apart), a file shorter than the offset
    /// (truncation), and different bytes where the last consumed line ended (a
    /// file rewritten in place and regrown past the offset, or an inode reused
    /// after its file was deleted).
    ///
    /// What it cannot see is a different file with the same id (any id, off
    /// unix) that is at least as long as the offset and ends a line at exactly
    /// that offset with the same last [`TAIL_LEN`] bytes. A record's unique id is
    /// at its start, not its end, so this needs a rewrite that reproduces the old
    /// line boundary — fixed-shape records in a log that was truncated and regrew
    /// past the offset between two refreshes. The cost is then an undercount, the
    /// records before the offset in the new file, never a double count.
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
    ///
    /// A log that exists but cannot be positioned in also yields `None`, which
    /// makes the first refresh scan it from its first byte: correct, but the cost
    /// this type exists to avoid, so it is worth a warning.
    fn baseline(path: &Path) -> Option<Self> {
        match Self::try_baseline(path) {
            Ok(cursor) => cursor,
            Err(e) => {
                tracing::warn!(
                    "github counters: cannot take a baseline of {}: {e}; the first summary \
                     will scan the whole log",
                    path.display()
                );
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
        let offset = end_of_last_line(&mut file, meta.len(), BASELINE_SCAN_LIMIT)?;
        let tail = read_tail(&mut file, offset)?;
        Ok(Some(Self {
            id: file_id(&meta),
            offset,
            tail,
        }))
    }
}

/// The offset just past the last `\n` in the first `len` bytes of `file`. Scans
/// backwards, so it reads only the final line, and gives up after `limit` bytes.
///
/// With no newline in the whole file the answer is 0, the start of its one
/// partial line. With none in the last `limit` bytes it is `len`: a baseline
/// inside a line that long, so the first read after it starts mid-record and
/// skips that fragment as malformed. That loses at most the one record, which is
/// the price of never reading more than `limit` bytes.
fn end_of_last_line(file: &mut File, len: u64, limit: u64) -> io::Result<u64> {
    let floor = len.saturating_sub(limit);
    let mut block = [0u8; BACKSCAN_BLOCK];
    let mut end = len;
    while end > floor {
        let start = end.saturating_sub(BACKSCAN_BLOCK as u64).max(floor);
        let want = usize::try_from(end - start).unwrap_or(BACKSCAN_BLOCK);
        let chunk = &mut block[..want];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(i) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(if floor == 0 { 0 } else { len })
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
        let live = match Open::at(&self.path) {
            Ok(live) => Some(live),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let Some(Plan { mut cursor, files }) = self.resume(live) else {
            // No log, so nothing in it: `aggregate`'s answer for a missing file. A
            // log created later is scanned from its first byte.
            return Ok(true);
        };
        let result = self.read_files(files, &mut cursor, stop);
        // Keep the cursor whatever happened: it is consistent at the last line
        // consumed, and a failed read should not cost a rebuild next time.
        self.cursor = Some(cursor);
        result
    }

    /// What to read, and from where: the stored cursor when its file can still be
    /// found — as the live file, or rotated to `log.jsonl.N` — otherwise a fresh
    /// cursor at the live file's first byte with the tally reset. `None` when
    /// there is no live file and nothing to follow, which leaves the tally empty.
    fn resume(&mut self, mut live: Option<Open>) -> Option<Plan> {
        if let Some(cursor) = self.cursor.take() {
            match self.locate(&cursor, live.as_mut()) {
                Ok(Some(mut files)) => {
                    files.extend(live);
                    return Some(Plan { cursor, files });
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(
                    "github counters: cannot verify the position in {}, rescanning: {e}",
                    self.path.display()
                ),
            }
        }
        // First sight of this file, or it was replaced, truncated, rewritten or
        // rotated out of retention: what was tallied came from something else.
        self.counts = GhCounts::default();
        live.map(|live| Plan {
            cursor: Cursor::start_of(live.id),
            files: vec![live],
        })
    }

    /// The rotated files to read before the live one, oldest first with the
    /// cursor's own file first — none at all when the cursor is on the live file
    /// — or `None` when the cursor's file cannot be trusted and the tally has to
    /// be rebuilt.
    ///
    /// A live file with the cursor's id that is not intact was truncated or
    /// rewritten in place, so there is nothing to follow. A live file with another
    /// id (or none, between a rotation and the next append) is a rotation or a
    /// `prune`; only a rotation leaves the cursor's file next to the log.
    fn locate(
        &self,
        cursor: &Cursor,
        mut live: Option<&mut Open>,
    ) -> io::Result<Option<Vec<Open>>> {
        if let Some(live) = live.as_deref_mut() {
            if live.id == cursor.id {
                let intact = cursor.is_intact(&mut live.file, live.id, live.len)?;
                return Ok(intact.then(Vec::new));
            }
        }
        if cursor.id.is_none() {
            // No file ids on this platform, so nothing identifies a rotated file.
            return Ok(None);
        }
        self.find_rotated(cursor, live.map(|live| live.id))
    }

    /// Looks for the cursor's file among the rotated siblings of the log and
    /// returns the files to read, oldest first, ending with it at the front.
    ///
    /// Files are identified by id, never by name: a rotation shifts every file up
    /// one name, possibly between two opens here. They only ever move *up*, so
    /// opening in ascending order sees every file that outlasts the search, some
    /// of them twice — which is why a file already seen (the live file included)
    /// is skipped. The first sighting of a file is then always newer than that of
    /// any older one, so the order seen, reversed, is the order written. A file
    /// the search misses leaves the cursor's file unfound, which rebuilds the
    /// tally: the same answer a prune gets, never a repeated record.
    fn find_rotated(&self, cursor: &Cursor, live: Option<FileId>) -> io::Result<Option<Vec<Open>>> {
        let mut seen: Vec<FileId> = live.into_iter().collect();
        let mut newer_first = Vec::new();
        for path in request_log::rotated_files(&self.path) {
            let mut rotated = match Open::at(&path) {
                Ok(rotated) => rotated,
                // Shifted or rotated out between the listing and now.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if seen.contains(&rotated.id) {
                continue;
            }
            seen.push(rotated.id);
            if rotated.id != cursor.id {
                newer_first.push(rotated);
                continue;
            }
            if !cursor.is_intact(&mut rotated.file, rotated.id, rotated.len)? {
                return Ok(None);
            }
            newer_first.push(rotated);
            newer_first.reverse();
            return Ok(Some(newer_first));
        }
        Ok(None)
    }

    /// Reads `files` in order, the first from the cursor and each later one from
    /// its first byte. Returns `false` when `stop` ended it early.
    ///
    /// The cursor moves to a file only when the one before it has been read to the
    /// end, so wherever this stops — `stop`, or a read error — it is consistent at
    /// a line boundary of the file being read, and the next refresh finds that
    /// file again wherever a rotation has since moved it. A rotated file is
    /// finished, so a half-written last line it may hold is never completed and is
    /// left behind with the file.
    fn read_files(
        &mut self,
        files: Vec<Open>,
        cursor: &mut Cursor,
        stop: &mut impl FnMut() -> bool,
    ) -> io::Result<bool> {
        for (n, mut open) in files.into_iter().enumerate() {
            if n > 0 {
                *cursor = Cursor::start_of(open.id);
            }
            if !self.read_new_lines(&mut open.file, cursor, stop)? {
                return Ok(false);
            }
        }
        Ok(true)
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

        /// Where rotation puts the file that is `n` rotations old.
        fn rotated(&self, n: u32) -> PathBuf {
            self.path.with_extension(format!("jsonl.{n}"))
        }

        /// Rotates with the writer's own function, keeping `keep` files.
        #[cfg(unix)]
        fn rotate(&self, keep: u32) {
            request_log::rotate(&self.path, keep).unwrap();
        }
    }

    /// What a full scan reports for `files` read oldest first as one log: the
    /// windowed scan summed over them, which is what a tally that followed a
    /// rotation must equal.
    fn full_scan_of(files: &[&Path]) -> GhCounts {
        let dir = tempfile::tempdir().unwrap();
        let joined = dir.path().join("joined.jsonl");
        let mut out = File::create(&joined).unwrap();
        for file in files {
            out.write_all(&std::fs::read(file).unwrap()).unwrap();
        }
        drop(out);
        aggregate(&joined, Some(since()), None, None)
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
            end_of_last_line(&mut file, log.len(), u64::MAX).unwrap(),
            first.len() as u64
        );
        assert_eq!(end_of_last_line(&mut file, 0, u64::MAX).unwrap(), 0);

        log.write(&long); // no newline anywhere
        let mut file = File::open(&log.path).unwrap();
        assert_eq!(end_of_last_line(&mut file, log.len(), u64::MAX).unwrap(), 0);
    }

    #[test]
    fn the_search_for_the_last_line_gives_up_at_its_limit() {
        let log = Log::new();
        let first = gh(1, &["pr", "list"], "cli");
        let long = "x".repeat(3 * BACKSCAN_BLOCK + 17);
        log.write(&format!("{first}{long}"));
        let mut file = File::open(&log.path).unwrap();

        // The newline is further back than the limit reaches: the baseline lands at
        // the end, inside the long line, having read no more than the limit.
        let limit = 2 * BACKSCAN_BLOCK as u64;
        assert_eq!(
            end_of_last_line(&mut file, log.len(), limit).unwrap(),
            log.len()
        );

        // A limit that does reach it finds it, as does one longer than the file.
        let reach = long.len() as u64 + 1;
        assert_eq!(
            end_of_last_line(&mut file, log.len(), reach).unwrap(),
            first.len() as u64
        );
    }

    #[test]
    fn a_baseline_inside_a_huge_line_skips_that_fragment_and_keeps_counting() {
        let log = Log::new();
        // More than the baseline looks back over, with no newline in it.
        let huge = "x".repeat(usize::try_from(BASELINE_SCAN_LIMIT).unwrap() + 1024);
        log.write(&format!("{}{huge}", gh(1, &["pr", "list"], "cli")));

        let mut tally = log.start();
        assert_eq!(tally.offset(), Some(log.len()));

        // The huge line is finished by its writer, and a real record follows it.
        log.append("\n");
        log.append(&gh(2, &["pr", "view"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 1);
        assert_eq!(tally.counts().by_subcommand["pr view"], 1);
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

    #[cfg(unix)]
    #[test]
    fn a_rotated_log_is_followed_to_its_replacement() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 1);

        // Appended after that refresh and before the rotation, so it is in `.1` and
        // nowhere in the live file.
        log.append(&gh(2, &["pr", "view"], "cli"));
        log.rotate(3);
        log.append(&gh(3, &["api", "graphql"], "daemon"));
        assert!(tally.refresh(never));

        // Nothing is lost: every record since the baseline, as a scan of both files
        // sees them. The live file alone holds one, which is what a rebuild reported.
        assert_eq!(tally.counts().total(), 3);
        assert_eq!(*tally.counts(), full_scan_of(&[&log.rotated(1), &log.path]));
        assert_eq!(log.full_scan().total(), 1);
        assert_eq!(tally.offset(), Some(log.len()));
    }

    #[cfg(not(unix))]
    #[test]
    fn without_file_ids_a_rotation_is_a_rebuild() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));

        // Nothing identifies the rotated file, so the live file is all there is.
        std::fs::rename(&log.path, log.rotated(1)).unwrap();
        log.append(&gh(2, &["api", "graphql"], "daemon"));
        assert!(tally.refresh(never));
        assert_eq!(*tally.counts(), log.full_scan());
        assert_eq!(tally.counts().total(), 1);
    }

    #[test]
    fn a_deleted_log_is_no_log() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 1);

        // Gone, and nowhere next to it either: no log, no counts.
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

    #[cfg(unix)]
    #[test]
    fn several_rotations_between_refreshes_are_read_in_order() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));

        // Three rotations before the next refresh. Each file holds a different
        // command a different number of times, so a file skipped, read twice or
        // read out of order changes the tally.
        log.append(&gh(2, &["pr", "view"], "cli"));
        log.rotate(3);
        for n in 3..5 {
            log.append(&gh(n, &["issue", "list"], "cli"));
        }
        log.rotate(3);
        for n in 5..8 {
            log.append(&gh(n, &["repo", "view"], "cli"));
        }
        log.rotate(3);
        for n in 8..12 {
            log.append(&gh(n, &["api", "graphql"], "daemon"));
        }
        assert!(tally.refresh(never));

        let counts = tally.counts();
        assert_eq!(counts.total(), 1 + 1 + 2 + 3 + 4);
        assert_eq!(counts.by_subcommand["pr list"], 1);
        assert_eq!(counts.by_subcommand["pr view"], 1);
        assert_eq!(counts.by_subcommand["issue list"], 2);
        assert_eq!(counts.by_subcommand["repo view"], 3);
        assert_eq!(
            *counts,
            full_scan_of(&[&log.rotated(3), &log.rotated(2), &log.rotated(1), &log.path])
        );
        assert_eq!(tally.offset(), Some(log.len()));

        // Nothing new: nothing recounted.
        let before = tally.counts().clone();
        assert!(tally.refresh(never));
        assert_eq!(*tally.counts(), before);
    }

    #[cfg(unix)]
    #[test]
    fn the_tally_matches_what_was_appended_across_many_rotations() {
        let log = Log::new();
        log.write(""); // a log to take a baseline in, so the first rotation is followed
        let mut tally = log.start();
        let mut appended = 0;
        let mut last = 0;
        for round in 0..12 {
            // One to three rotations a refresh, the most a keep count of three can
            // follow, with a varying number of records in each file.
            for rotation in 0..=round % 3 {
                for _ in 0..=(round + rotation) % 4 {
                    appended += 1;
                    log.append(&gh(appended, &["pr", "list"], "cli"));
                }
                log.rotate(3);
            }
            appended += 1;
            log.append(&gh(appended, &["pr", "list"], "cli"));

            assert!(tally.refresh(never));
            let total = tally.counts().total();
            assert!(total >= last, "round {round}: {total} after {last}");
            assert_eq!(total, u64::try_from(appended).unwrap(), "round {round}");
            last = total;
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_cursor_file_rotated_out_of_retention_is_rebuilt_from_the_live_file() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));

        // Keeping one file, two rotations push the cursor's file out. What is left
        // next to the log is another file, with records in the window: reading it
        // would count what the daemon never had a cursor in.
        log.append(&gh(2, &["pr", "list"], "cli"));
        log.rotate(1);
        log.append(&gh(3, &["pr", "view"], "cli"));
        log.append(&gh(4, &["pr", "view"], "cli"));
        log.rotate(1);
        log.append(&gh(5, &["api", "graphql"], "daemon"));
        assert!(tally.refresh(never));

        assert_eq!(tally.counts().total(), 1);
        assert_eq!(*tally.counts(), log.full_scan());
        assert_eq!(tally.offset(), Some(log.len()));
    }

    #[cfg(unix)]
    #[test]
    fn a_rotation_that_keeps_no_files_is_rebuilt_from_the_new_log() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        log.append(&gh(2, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 2);

        // With `OMNI_DEV_LOG_KEEP_FILES=0` rotation deletes the file.
        log.rotate(0);
        assert!(!log.path.exists());
        log.append(&gh(3, &["api", "graphql"], "daemon"));
        assert!(tally.refresh(never));

        assert_eq!(tally.counts().total(), 1);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[cfg(unix)]
    #[test]
    fn a_prune_is_rebuilt_even_when_rotated_files_exist() {
        let log = Log::new();
        // An earlier rotation left a file of in-window records that are not the
        // daemon's: its baseline is taken after them.
        let earlier: String = (1..4).map(|n| gh(n, &["issue", "list"], "cli")).collect();
        log.write(&earlier);
        log.rotate(3);
        log.write("");
        let mut tally = log.start();
        log.append(&gh(10, &["pr", "list"], "cli"));
        log.append(&gh(11, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 2);

        // A prune unlinks the file the cursor was in, so it is nowhere to be found.
        log.replace(&format!(
            "{}{}",
            gh(12, &["api", "graphql"], "daemon"),
            gh(13, &["pr", "view"], "cli"),
        ));
        assert!(tally.refresh(never));

        assert_eq!(tally.counts().total(), 2);
        assert!(!tally.counts().by_subcommand.contains_key("issue list"));
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[cfg(unix)]
    #[test]
    fn a_rotated_file_that_no_longer_matches_the_cursor_is_not_trusted() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        log.append(&gh(2, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));
        let stored = tally.offset().unwrap();

        log.rotate(3);
        log.append(&gh(3, &["api", "graphql"], "daemon"));

        // The cursor's file is still `.1`, by id, but rewritten in place and longer
        // than the stored offset: its bytes before the offset are not the ones read.
        let rewritten: String = (10..16).map(|n| gh(n, &["issue", "list"], "cli")).collect();
        std::fs::write(log.rotated(1), rewritten).unwrap();
        assert!(std::fs::metadata(log.rotated(1)).unwrap().len() > stored);
        assert!(tally.refresh(never));

        assert_eq!(tally.counts().total(), 1);
        assert_eq!(*tally.counts(), log.full_scan());
    }

    #[cfg(unix)]
    #[test]
    fn a_log_rotated_away_and_not_yet_recreated_is_followed() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));

        // Rotated, and the next append has not created the new log yet. The
        // records the last refresh missed are in `.1`, and the tally keeps them.
        log.append(&gh(2, &["pr", "view"], "cli"));
        log.append(&gh(3, &["pr", "view"], "cli"));
        log.rotate(3);
        assert!(!log.path.exists());
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 3);
        assert_eq!(*tally.counts(), full_scan_of(&[&log.rotated(1)]));
        assert_eq!(
            tally.offset(),
            Some(std::fs::metadata(log.rotated(1)).unwrap().len())
        );

        // Asked again before the new log exists: still nothing lost, nothing twice.
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 3);

        // The new log appears; the tally moves onto it.
        log.append(&gh(4, &["api", "graphql"], "daemon"));
        log.append(&gh(5, &["api", "graphql"], "daemon"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 5);
        assert_eq!(*tally.counts(), full_scan_of(&[&log.rotated(1), &log.path]));
        assert_eq!(tally.offset(), Some(log.len()));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_seen_at_two_paths_is_read_once() {
        let log = Log::new();
        let mut tally = log.start();
        log.append(&gh(1, &["pr", "list"], "cli"));
        assert!(tally.refresh(never));

        log.rotate(3);
        log.append(&gh(2, &["pr", "view"], "cli"));

        // What a rotation that lands while the files are being looked at can show:
        // the live file again, under `.1`, with the cursor's file one name further
        // up. Read once for each path, its record would count twice.
        std::fs::rename(log.rotated(1), log.rotated(2)).unwrap();
        std::fs::hard_link(&log.path, log.rotated(1)).unwrap();
        assert!(tally.refresh(never));

        assert_eq!(tally.counts().total(), 2);
        assert_eq!(tally.counts().by_subcommand["pr view"], 1);
        assert_eq!(tally.offset(), Some(log.len()));
    }

    #[cfg(unix)]
    #[test]
    fn a_refresh_stopped_inside_a_rotated_file_resumes_there() {
        let log = Log::new();
        log.write(""); // a log to take a baseline in, so the first rotation is followed
        let mut tally = log.start();
        assert!(tally.refresh(never));

        for n in 1..=4 {
            log.append(&gh(n, &["pr", "list"], "cli"));
        }
        log.rotate(3);
        for n in 5..=6 {
            log.append(&gh(n, &["pr", "view"], "cli"));
        }

        // Two lines of the rotated file, then stop.
        let mut calls = 0;
        assert!(!tally.refresh(|| {
            calls += 1;
            calls > 2
        }));
        assert_eq!(tally.counts().total(), 2);

        // Another rotation moves the file the cursor is in, before it resumes.
        log.rotate(3);
        log.append(&gh(7, &["api", "graphql"], "daemon"));
        assert!(tally.refresh(never));
        assert_eq!(tally.counts().total(), 7);
        assert_eq!(
            *tally.counts(),
            full_scan_of(&[&log.rotated(2), &log.rotated(1), &log.path])
        );
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
