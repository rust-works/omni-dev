//! Source comment markers that exclude or tolerate a *region* of a file in
//! `coverage diff`.
//!
//! `--ignore-filename-regex` (and its `coverage.yaml` twin) drops a **whole
//! file** from both reports, but the noise it exists to silence is almost always
//! narrower than a file: one function gated on a *runtime* CPU-feature check is
//! compiled into the denominator on every run yet executed only on a host that
//! has the instruction, so it flips whenever the baseline and head runs draw
//! different runner CPUs. Excluding the file hides far more real coverage than
//! noise.
//!
//! Naming the region in config is not an option either: line numbers are
//! invalidated by every edit above them, and function *extents* are absent from
//! the lcov `FN:` records (start line + mangled symbol only). So the region is
//! delimited in the source itself, and each revision's own source is scanned —
//! head from the worktree, base from the base blob — which means no line number
//! is ever stored, and a region that moves, grows, or disappears between base
//! and head is handled by construction.
//!
//! A region is opened by a comment naming a kind and a mandatory reason, and
//! closed by an `end` comment; there is also a single-line form. **The syntax is
//! documented, with examples, in `docs/coverage.md`** — deliberately not here:
//! see [`INTRODUCER`] for why this file must not contain a literal marker.
//!
//! Two kinds, differing in what they do to the *reports*:
//!
//! - [`MarkerKind::Ignore`] — the lines are removed from **both** reports before
//!   any analysis, so they cannot move any number. The scoped twin of
//!   `ignore-filename-regex`.
//! - [`MarkerKind::Tolerate`] — the lines stay, so the percentage stays honest;
//!   only the *delta signals* are masked, by scoring each tolerated head line
//!   with its baseline hit status.
//!
//! Matching is a **plain substring** on any line, so it works in any comment
//! syntax and never depends on parsing the host language. A marker inside a
//! string literal is therefore matched too — accepted, and documented.

use std::collections::BTreeSet;

use anyhow::{bail, Result};

/// The literal that introduces every marker.
///
/// Assembled with `concat!` rather than written out, because omni-dev measures
/// its own coverage: this file is in its own report, so a contiguous introducer
/// anywhere in this source — a doc example, a test fixture — would be scanned as
/// a real marker, and the deliberately-malformed fixtures below would fail
/// omni-dev's own `coverage diff` run outright. `self_source_contains_no_literal_introducer`
/// pins that invariant; put examples in `docs/coverage.md`, which is not a
/// source file and is never scanned.
///
/// A file that does not contain this substring anywhere cannot carry a marker,
/// and so cannot raise a marker error either — which is what makes the
/// whole-file short-circuit in [`scan`] exact rather than merely fast.
pub const INTRODUCER: &str = concat!("omni-dev", ": coverage");

/// What a marked region does to the reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkerKind {
    /// Remove the region's lines from both reports before analysis.
    Ignore,
    /// Keep the region's lines, but mask coverage *flips* on them.
    Tolerate,
}

impl MarkerKind {
    /// The lowercase keyword used in the marker and in rendered output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Tolerate => "tolerate",
        }
    }
}

/// One marked region, as a 1-based inclusive line span.
///
/// Both marker lines are inside the span. They are comments, so they are never
/// executable and never appear in a coverage report — including them costs
/// nothing and keeps the span identical to what a reader sees in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// Whether the region is ignored or tolerated.
    pub kind: MarkerKind,
    /// First line of the region (the opening marker's own line).
    pub start: u32,
    /// Last line of the region (the `end` marker's line, or `start` for the
    /// single-line form).
    pub end: u32,
    /// The mandatory reason text explaining why the region is silenced.
    pub reason: String,
}

/// The regions of one file, plus the line sets they expand to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMarkers {
    /// Every line covered by an [`MarkerKind::Ignore`] region.
    pub ignored: BTreeSet<u32>,
    /// Every line covered by a [`MarkerKind::Tolerate`] region.
    pub tolerated: BTreeSet<u32>,
    /// The regions themselves, in source order, for reporting.
    pub regions: Vec<Region>,
}

impl FileMarkers {
    /// Expands `regions` into the per-line sets.
    pub fn new(regions: Vec<Region>) -> Self {
        let mut ignored = BTreeSet::new();
        let mut tolerated = BTreeSet::new();
        for region in &regions {
            let set = match region.kind {
                MarkerKind::Ignore => &mut ignored,
                MarkerKind::Tolerate => &mut tolerated,
            };
            set.extend(region.start..=region.end);
        }
        Self {
            ignored,
            tolerated,
            regions,
        }
    }

    /// Whether this file carries no markers at all.
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

/// A marker keyword, its kind, and whether it is the single-line form. Ordered
/// longest-first so `ignore-line` is never read as `ignore` plus trailing junk.
const KEYWORDS: &[(&str, MarkerKind, bool)] = &[
    ("ignore-line", MarkerKind::Ignore, true),
    ("tolerate-line", MarkerKind::Tolerate, true),
    ("ignore", MarkerKind::Ignore, false),
    ("tolerate", MarkerKind::Tolerate, false),
];

/// A region start that has not been closed yet.
struct Open {
    kind: MarkerKind,
    start: u32,
    reason: String,
}

/// Scans `text` for coverage markers, returning the regions in source order.
///
/// `path` is used only to build error messages. Every malformed marker is a hard
/// error naming `path:line` rather than a silent skip: a marker that does not
/// take effect is worse than one that does not exist, because its author
/// believes the noise is silenced.
pub fn scan(path: &str, text: &str) -> Result<Vec<Region>> {
    // A file with no introducer cannot produce a region *or* an error, so this
    // short-circuit changes nothing but the cost of the common case.
    if !text.contains(INTRODUCER) {
        return Ok(Vec::new());
    }

    let mut regions = Vec::new();
    let mut open: Option<Open> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let Some(rest) = raw.trim_end_matches('\r').split(INTRODUCER).nth(1) else {
            continue;
        };
        let rest = rest.trim_start();

        if let Some(tail) = strip_keyword(rest, "end") {
            if !tail.trim().is_empty() {
                bail!(
                    "{path}:{line}: unexpected text after `{INTRODUCER} end`: `{}`",
                    tail.trim()
                );
            }
            let Some(open) = open.take() else {
                bail!("{path}:{line}: `{INTRODUCER} end` without a matching region start");
            };
            regions.push(Region {
                kind: open.kind,
                start: open.start,
                end: line,
                reason: open.reason,
            });
            continue;
        }

        let Some((keyword, kind, single)) = KEYWORDS
            .iter()
            .find(|(keyword, _, _)| strip_keyword(rest, keyword).is_some())
            .copied()
        else {
            bail!(
                "{path}:{line}: unrecognised coverage marker `{INTRODUCER} {}` \
                 (expected `ignore`, `tolerate`, `ignore-line`, `tolerate-line`, or `end`)",
                rest.split_whitespace().next().unwrap_or("")
            );
        };
        // `strip_keyword` just succeeded for this keyword.
        let tail = strip_keyword(rest, keyword).unwrap_or("");
        let reason = parse_reason(path, line, keyword, tail)?;

        if single {
            regions.push(Region {
                kind,
                start: line,
                end: line,
                reason,
            });
            continue;
        }

        if let Some(previous) = &open {
            bail!(
                "{path}:{line}: nested coverage region; the `{}` region opened at line {} is \
                 still open (regions may not overlap)",
                previous.kind.as_str(),
                previous.start
            );
        }
        open = Some(Open {
            kind,
            start: line,
            reason,
        });
    }

    if let Some(open) = open {
        bail!(
            "{path}:{}: unterminated `{INTRODUCER} {}` region (add `{INTRODUCER} end`)",
            open.start,
            open.kind.as_str()
        );
    }

    Ok(regions)
}

/// Strips `keyword` from the front of `rest`, requiring it to be followed by
/// whitespace or end-of-line so `ignore` never matches the prefix of
/// `ignore-line`.
fn strip_keyword<'a>(rest: &'a str, keyword: &str) -> Option<&'a str> {
    let tail = rest.strip_prefix(keyword)?;
    if tail.is_empty() || tail.starts_with(|c: char| c.is_whitespace()) {
        Some(tail)
    } else {
        None
    }
}

/// Extracts the mandatory `reason="…"` from a marker's tail.
///
/// The reason is required because silencing must be explained at the site: a
/// bare marker tells a later reader nothing about whether the noise it hides is
/// still real.
fn parse_reason(path: &str, line: u32, keyword: &str, tail: &str) -> Result<String> {
    let tail = tail.trim();
    let Some(after) = tail.split_once("reason=\"").map(|(_, after)| after) else {
        bail!(
            "{path}:{line}: `{INTRODUCER} {keyword}` needs a reason \
             (write `{INTRODUCER} {keyword} reason=\"why this is silenced\"`)"
        );
    };
    let Some((reason, _)) = after.split_once('"') else {
        bail!("{path}:{line}: unterminated `reason=\"…\"` (missing closing quote)");
    };
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("{path}:{line}: `reason=\"\"` is empty; explain why the region is silenced");
    }
    Ok(reason.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Builds a marker line. Every fixture goes through this rather than writing
    /// the introducer out, so this file's own source stays marker-free — see
    /// [`INTRODUCER`] and `self_source_contains_no_literal_introducer`.
    fn mark(comment: &str, rest: &str) -> String {
        format!("{comment} {INTRODUCER} {rest}")
    }

    /// Joins fixture lines into a file body with a trailing newline.
    fn file(lines: &[String]) -> String {
        format!("{}\n", lines.join("\n"))
    }

    /// A `//`-commented marker line, the common case.
    fn rs(rest: &str) -> String {
        mark("//", rest)
    }

    /// Scans and unwraps, for the cases that must succeed.
    fn ok(text: &str) -> Vec<Region> {
        scan("src/a.rs", text).unwrap()
    }

    /// Scans and returns the error message, for the cases that must fail.
    fn err(text: &str) -> String {
        scan("src/a.rs", text).unwrap_err().to_string()
    }

    fn region(kind: MarkerKind, start: u32, end: u32, reason: &str) -> Region {
        Region {
            kind,
            start,
            end,
            reason: reason.to_string(),
        }
    }

    /// The guard that keeps omni-dev able to measure its own coverage. A literal
    /// introducer anywhere in this file would be scanned as a real marker when
    /// omni-dev runs `coverage diff` on itself, and the malformed fixtures below
    /// would then fail that run. Examples belong in `docs/coverage.md`.
    #[test]
    fn self_source_contains_no_literal_introducer() {
        let source = include_str!("markers.rs");
        assert!(
            !source.contains(INTRODUCER),
            "src/coverage/markers.rs must not contain a literal marker introducer; \
             build fixtures with `mark()` and put examples in docs/coverage.md"
        );
    }

    #[test]
    fn file_without_the_introducer_yields_nothing() {
        assert!(ok("fn a() {}\n// ordinary comment\n").is_empty());
    }

    #[test]
    fn scans_an_ignore_region() {
        let text = file(&[
            "fn a() {}".to_string(),
            rs("ignore reason=\"CPU-gated\""),
            "fn b() {}".to_string(),
            rs("end"),
            "fn c() {}".to_string(),
        ]);
        assert_eq!(
            ok(&text),
            vec![region(MarkerKind::Ignore, 2, 4, "CPU-gated")]
        );
    }

    #[test]
    fn scans_a_tolerate_region() {
        let text = file(&[
            rs("tolerate reason=\"avx512f arm\""),
            "fn b() {}".to_string(),
            rs("end"),
        ]);
        assert_eq!(
            ok(&text),
            vec![region(MarkerKind::Tolerate, 1, 3, "avx512f arm")]
        );
    }

    #[test]
    fn scans_both_single_line_forms() {
        let text = file(&[
            rs("ignore-line reason=\"one off\""),
            rs("tolerate-line reason=\"flaky\""),
        ]);
        assert_eq!(
            ok(&text),
            vec![
                region(MarkerKind::Ignore, 1, 1, "one off"),
                region(MarkerKind::Tolerate, 2, 2, "flaky"),
            ]
        );
    }

    /// `ignore-line` must not be read as `ignore` with trailing junk — the
    /// keyword table is longest-first *and* `strip_keyword` requires a word
    /// boundary, so either alone would be enough.
    #[test]
    fn single_line_keyword_wins_over_its_prefix() {
        let text = file(&[rs("ignore-line reason=\"one off\"")]);
        let regions = ok(&text);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].end, 1, "must not open a region");
    }

    #[test]
    fn regions_may_repeat_within_a_file() {
        let text = file(&[
            rs("ignore reason=\"a\""),
            "x".to_string(),
            rs("end"),
            "y".to_string(),
            rs("tolerate reason=\"b\""),
            "z".to_string(),
            rs("end"),
        ]);
        assert_eq!(
            ok(&text),
            vec![
                region(MarkerKind::Ignore, 1, 3, "a"),
                region(MarkerKind::Tolerate, 5, 7, "b"),
            ]
        );
    }

    /// Matching is a plain substring, so any comment syntax works.
    #[test]
    fn any_comment_syntax_matches() {
        let text = file(&[
            mark("#", "ignore-line reason=\"shell\""),
            mark("<!--", "ignore-line reason=\"html\" -->"),
        ]);
        assert_eq!(ok(&text).len(), 2);
    }

    /// The flip side of a plain substring match: a marker inside a *string
    /// literal* is matched too, because nothing here parses the host language.
    /// Documented behaviour, not an oversight.
    #[test]
    fn marker_inside_a_string_literal_is_matched() {
        let text = file(&[mark("let s = '", "ignore-line reason=\"in a literal\"';")]);
        assert_eq!(ok(&text).len(), 1);
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let lines = [rs("ignore reason=\"crlf\""), "x".to_string(), rs("end")];
        let text = format!("{}\r\n", lines.join("\r\n"));
        assert_eq!(ok(&text), vec![region(MarkerKind::Ignore, 1, 3, "crlf")]);
    }

    #[test]
    fn marker_on_the_last_line_without_a_trailing_newline() {
        let text = format!("x\n{}", rs("ignore-line reason=\"last\""));
        assert_eq!(ok(&text), vec![region(MarkerKind::Ignore, 2, 2, "last")]);
    }

    #[test]
    fn reason_is_mandatory() {
        let text = file(&[rs("ignore"), "x".to_string(), rs("end")]);
        let message = err(&text);
        assert!(message.contains("src/a.rs:1"), "{message}");
        assert!(message.contains("needs a reason"), "{message}");
    }

    #[test]
    fn reason_must_not_be_empty() {
        let message = err(&file(&[rs("ignore-line reason=\"\"")]));
        assert!(message.contains("is empty"), "{message}");
    }

    #[test]
    fn reason_quote_must_be_closed() {
        let message = err(&file(&[rs("ignore-line reason=\"unclosed")]));
        assert!(message.contains("unterminated `reason"), "{message}");
    }

    #[test]
    fn nested_regions_are_rejected() {
        let text = file(&[
            rs("ignore reason=\"a\""),
            rs("tolerate reason=\"b\""),
            rs("end"),
        ]);
        let message = err(&text);
        assert!(message.contains("src/a.rs:2"), "{message}");
        assert!(message.contains("nested"), "{message}");
        assert!(message.contains("opened at line 1"), "{message}");
    }

    #[test]
    fn stray_end_is_rejected() {
        let message = err(&file(&["x".to_string(), rs("end")]));
        assert!(message.contains("src/a.rs:2"), "{message}");
        assert!(
            message.contains("without a matching region start"),
            "{message}"
        );
    }

    /// An unterminated region must never widen silently to end-of-file: that
    /// would silence an unbounded amount of code its author never looked at.
    #[test]
    fn unterminated_region_is_rejected() {
        let text = file(&[rs("ignore reason=\"a\""), "x".to_string(), "y".to_string()]);
        let message = err(&text);
        assert!(message.contains("src/a.rs:1"), "{message}");
        assert!(message.contains("unterminated"), "{message}");
    }

    #[test]
    fn unknown_keyword_is_rejected() {
        let message = err(&file(&[rs("skip reason=\"a\"")]));
        assert!(message.contains("unrecognised"), "{message}");
        assert!(message.contains("skip"), "{message}");
    }

    #[test]
    fn text_after_end_is_rejected() {
        let text = file(&[rs("ignore reason=\"a\""), rs("end reason=\"b\"")]);
        let message = err(&text);
        assert!(message.contains("unexpected text after"), "{message}");
    }

    #[test]
    fn file_markers_expand_regions_to_line_sets() {
        let markers = FileMarkers::new(vec![
            region(MarkerKind::Ignore, 2, 4, "a"),
            region(MarkerKind::Tolerate, 7, 7, "b"),
        ]);
        assert_eq!(markers.ignored, BTreeSet::from([2, 3, 4]));
        assert_eq!(markers.tolerated, BTreeSet::from([7]));
        assert!(!markers.is_empty());
        assert!(FileMarkers::default().is_empty());
    }
}
