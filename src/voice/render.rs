//! Rendering helpers for [`crate::voice::TranscriptEvent`] streams.
//!
//! Two output formats are supported, both designed to stream — each event
//! is flushed as soon as the backend emits it so a slow transcriber gives
//! incremental feedback on stdout instead of buffering the full transcript.
//!
//! * `Jsonl` — one `serde_json` line per event. Stable, machine-readable.
//! * `Md` — human-readable transcript. Consecutive `Final` events from the
//!   same speaker collapse into a single paragraph prefixed with
//!   `[HH:MM:SS] **speaker**: ` (the `**speaker**: ` prefix is omitted
//!   when no speaker is attached).
//!
//! Lives under `src/voice/` rather than `src/cli/voice/` because the
//! `voice review` command in #804 will reuse `render_markdown`.
//!
//! `Partial` and `Endpoint` events are skipped in markdown mode: the batch
//! backend in #801 emits no partials, and endpoint markers add no signal
//! to a reader-oriented transcript.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;

use crate::voice::transcriber::{SpeakerId, TranscriptEvent};

/// Output format selector. The CLI maps `--format md|jsonl` directly onto
/// this; the default value is chosen at runtime by [`detect_format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// JSON Lines — one event per line, machine-readable.
    Jsonl,
    /// Markdown — human-readable transcript view.
    Md,
}

/// Resolves the effective output format.
///
/// Explicit `--format` always wins. With no flag, stdout-on-a-tty defaults
/// to markdown; stdout-on-a-pipe defaults to JSONL. Mirrors the design
/// principle that machine consumers (`| jq`, file redirection) get a
/// stable schema and humans get prose.
pub fn detect_format(explicit: Option<OutputFormat>, stdout_is_tty: bool) -> OutputFormat {
    match explicit {
        Some(fmt) => fmt,
        None if stdout_is_tty => OutputFormat::Md,
        None => OutputFormat::Jsonl,
    }
}

/// Streams events as JSON Lines to `w`, flushing after each event.
///
/// The flush is deliberate: a streaming backend can emit a `Partial`
/// half a second before its `Final` and a downstream `tail -f` consumer
/// should see it without waiting for the next event to push it through
/// stdio buffering.
pub fn render_jsonl<I, W>(events: I, w: &mut W) -> Result<()>
where
    I: IntoIterator<Item = Result<TranscriptEvent>>,
    W: Write,
{
    for event in events {
        let event = event?;
        serde_json::to_writer(&mut *w, &event)?;
        writeln!(w)?;
        w.flush()?;
    }
    Ok(())
}

/// Streams events as Markdown to `w`.
///
/// Groups consecutive `Final` events by speaker into one paragraph each
/// (a paragraph being one timestamped line followed by a blank line),
/// flushing at every speaker-paragraph boundary so the reader sees
/// transcript output as it commits. `Partial` and `Endpoint` events are
/// dropped.
pub fn render_markdown<I, W>(events: I, w: &mut W) -> Result<()>
where
    I: IntoIterator<Item = Result<TranscriptEvent>>,
    W: Write,
{
    let mut group: Option<MarkdownParagraph> = None;
    for event in events {
        let event = event?;
        let TranscriptEvent::Final {
            text,
            start,
            speaker,
            ..
        } = event
        else {
            continue;
        };
        match group.as_mut() {
            Some(existing) if existing.speaker == speaker => {
                existing.push_segment(&text);
            }
            _ => {
                if let Some(prev) = group.take() {
                    prev.write(w)?;
                    w.flush()?;
                }
                group = Some(MarkdownParagraph::new(start, speaker, text));
            }
        }
    }
    if let Some(last) = group {
        last.write(w)?;
        w.flush()?;
    }
    Ok(())
}

struct MarkdownParagraph {
    start: Duration,
    speaker: Option<SpeakerId>,
    text: String,
}

impl MarkdownParagraph {
    fn new(start: Duration, speaker: Option<SpeakerId>, text: String) -> Self {
        Self {
            start,
            speaker,
            text,
        }
    }

    fn push_segment(&mut self, segment: &str) {
        // Single space joiner — matches how a human would read aloud
        // consecutive finals from one speaker. Don't collapse internal
        // whitespace; the backend's text is authoritative.
        if !self.text.is_empty() && !segment.is_empty() {
            self.text.push(' ');
        }
        self.text.push_str(segment);
    }

    fn write<W: Write>(&self, w: &mut W) -> Result<()> {
        let ts = fmt_timestamp(self.start);
        match self.speaker.as_deref() {
            Some(name) => writeln!(w, "[{ts}] **{name}**: {}", self.text)?,
            None => writeln!(w, "[{ts}] {}", self.text)?,
        }
        writeln!(w)?;
        Ok(())
    }
}

/// Formats a stream-relative `Duration` as `HH:MM:SS`, zero-padded, with
/// hours always shown.
///
/// Hours always shown — even for short audio — so the column width is
/// stable across a transcript and matches the convention used by
/// `ffmpeg`, `mpv`, and most media players.
fn fmt_timestamp(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::transcriber::EndpointKind;

    /// `Write` impl that fails on every `write` call. Lets tests exercise
    /// the first `?` site in any rendering function without depending on
    /// the exact number of internal writes a formatter issues.
    struct AlwaysFailWriter;

    impl Write for AlwaysFailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("forced write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// `Write` impl that accepts all writes but fails on `flush`. Targets
    /// the post-event `w.flush()?` branches in `render_jsonl` and
    /// `render_markdown`'s paragraph boundaries.
    struct FlushFailWriter;

    impl Write for FlushFailWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("forced flush failure"))
        }
    }

    fn final_event(text: &str, start_secs: u64, speaker: Option<&str>) -> TranscriptEvent {
        TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, u128::from(start_secs) + 1),
            text: text.to_string(),
            start: Duration::from_secs(start_secs),
            end: Duration::from_secs(start_secs + 1),
            confidence: 0.9,
            words: None,
            speaker: speaker.map(str::to_string),
            revisable: false,
        }
    }

    fn render_md_to_string<I>(events: I) -> String
    where
        I: IntoIterator<Item = TranscriptEvent>,
    {
        let mut buf: Vec<u8> = Vec::new();
        render_markdown(events.into_iter().map(Ok), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn render_jsonl_to_string<I>(events: I) -> String
    where
        I: IntoIterator<Item = TranscriptEvent>,
    {
        let mut buf: Vec<u8> = Vec::new();
        render_jsonl(events.into_iter().map(Ok), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn detect_format_explicit_wins_over_tty() {
        assert_eq!(
            detect_format(Some(OutputFormat::Jsonl), true),
            OutputFormat::Jsonl
        );
        assert_eq!(
            detect_format(Some(OutputFormat::Md), false),
            OutputFormat::Md
        );
    }

    #[test]
    fn detect_format_tty_defaults_to_md() {
        assert_eq!(detect_format(None, true), OutputFormat::Md);
    }

    #[test]
    fn detect_format_pipe_defaults_to_jsonl() {
        assert_eq!(detect_format(None, false), OutputFormat::Jsonl);
    }

    #[test]
    fn fmt_timestamp_pads_zero() {
        assert_eq!(fmt_timestamp(Duration::from_secs(0)), "00:00:00");
        assert_eq!(fmt_timestamp(Duration::from_secs(7)), "00:00:07");
        assert_eq!(fmt_timestamp(Duration::from_secs(75)), "00:01:15");
        assert_eq!(fmt_timestamp(Duration::from_secs(3_600)), "01:00:00");
        assert_eq!(
            fmt_timestamp(Duration::from_secs(3_600 + 23 * 60 + 45)),
            "01:23:45"
        );
    }

    #[test]
    fn fmt_timestamp_truncates_subsecond() {
        // 1.9s reads as 00:00:01 (whole seconds only, matching media-player
        // convention).
        assert_eq!(fmt_timestamp(Duration::from_millis(1_900)), "00:00:01");
    }

    #[test]
    fn render_jsonl_emits_one_line_per_event_and_flushes() {
        let out = render_jsonl_to_string([
            final_event("hello", 0, None),
            TranscriptEvent::Endpoint {
                at: Duration::from_millis(1_500),
                kind: EndpointKind::StreamEnd,
            },
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(r#"{"type":"final""#));
        assert!(lines[0].contains(r#""text":"hello""#));
        assert!(lines[1].starts_with(r#"{"type":"endpoint""#));
        assert!(lines[1].contains(r#""at":1.5"#));
        // Trailing newline after the last event.
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn render_jsonl_propagates_stream_error() {
        let events: Vec<Result<TranscriptEvent>> = vec![Err(anyhow::anyhow!("backend exploded"))];
        let mut buf: Vec<u8> = Vec::new();
        let err = render_jsonl(events, &mut buf).unwrap_err();
        assert!(err.to_string().contains("backend exploded"));
    }

    #[test]
    fn render_markdown_propagates_stream_error() {
        let events: Vec<Result<TranscriptEvent>> = vec![Err(anyhow::anyhow!("backend exploded"))];
        let mut buf: Vec<u8> = Vec::new();
        let err = render_markdown(events, &mut buf).unwrap_err();
        assert!(err.to_string().contains("backend exploded"));
    }

    #[test]
    fn render_markdown_handles_empty_text_segment_in_group() {
        // Two consecutive same-speaker finals where the *second* has empty
        // text. The push_segment joiner must skip the leading space; the
        // output should be a single paragraph with the first final's text
        // unchanged.
        let out = render_md_to_string([
            final_event("hello", 0, Some("alice")),
            final_event("", 2, Some("alice")),
        ]);
        assert_eq!(out, "[00:00:00] **alice**: hello\n\n");
    }

    #[test]
    fn render_markdown_skips_partial_and_endpoint() {
        let out = render_md_to_string([
            TranscriptEvent::Partial {
                text: "ignored".into(),
                start: Duration::from_secs(0),
                end: Duration::from_secs(1),
                words: None,
                speaker: None,
            },
            final_event("kept", 0, None),
            TranscriptEvent::Endpoint {
                at: Duration::from_secs(2),
                kind: EndpointKind::StreamEnd,
            },
        ]);
        assert!(!out.contains("ignored"));
        assert!(out.contains("kept"));
        // No timestamp from the endpoint should bleed through.
        assert!(!out.contains("[00:00:02]"));
    }

    #[test]
    fn render_markdown_groups_consecutive_same_speaker_finals() {
        let out = render_md_to_string([
            final_event("hello", 0, Some("alice")),
            final_event("world", 2, Some("alice")),
            final_event("hi", 4, Some("bob")),
        ]);
        // alice's two segments merge onto one line; bob is a new paragraph.
        let expected = "[00:00:00] **alice**: hello world\n\n[00:00:04] **bob**: hi\n\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_markdown_groups_consecutive_none_speaker_finals() {
        let out =
            render_md_to_string([final_event("alpha", 0, None), final_event("beta", 3, None)]);
        // Two consecutive None-speaker finals collapse into one paragraph
        // with no speaker prefix.
        assert_eq!(out, "[00:00:00] alpha beta\n\n");
    }

    #[test]
    fn render_markdown_speaker_change_starts_new_paragraph() {
        let out = render_md_to_string([
            final_event("a", 0, Some("alice")),
            final_event("b", 1, None),
            final_event("c", 2, Some("alice")),
        ]);
        // None → alice → None all distinct paragraphs.
        let expected = "[00:00:00] **alice**: a\n\n[00:00:01] b\n\n[00:00:02] **alice**: c\n\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_jsonl_propagates_writer_error() {
        let events = [Ok(final_event("a", 0, None))];
        let err = render_jsonl(events, &mut AlwaysFailWriter).unwrap_err();
        assert!(err.to_string().contains("forced write failure"));
    }

    #[test]
    fn render_jsonl_propagates_flush_error() {
        let events = [Ok(final_event("a", 0, None))];
        let err = render_jsonl(events, &mut FlushFailWriter).unwrap_err();
        assert!(err.to_string().contains("forced flush failure"));
    }

    #[test]
    fn render_markdown_propagates_writer_error_with_speaker() {
        // Exercises the paragraph-write `?` plus the with-speaker writeln
        // arm inside MarkdownParagraph::write.
        let events = [Ok(final_event("a", 0, Some("alice")))];
        let err = render_markdown(events, &mut AlwaysFailWriter).unwrap_err();
        assert!(err.to_string().contains("forced write failure"));
    }

    #[test]
    fn render_markdown_propagates_writer_error_no_speaker() {
        // Same as above, but exercises the no-speaker writeln arm.
        let events = [Ok(final_event("a", 0, None))];
        let err = render_markdown(events, &mut AlwaysFailWriter).unwrap_err();
        assert!(err.to_string().contains("forced write failure"));
    }

    #[test]
    fn render_markdown_propagates_flush_error_at_paragraph_end() {
        // FlushFailWriter accepts the paragraph's writes (content +
        // blank line), then fails on the trailing per-paragraph flush.
        let events = [Ok(final_event("a", 0, None))];
        let err = render_markdown(events, &mut FlushFailWriter).unwrap_err();
        assert!(err.to_string().contains("forced flush failure"));
    }

    #[test]
    fn render_markdown_propagates_writer_error_at_paragraph_break() {
        // Two different-speaker finals exercise the mid-stream
        // `prev.write(w)?` path (line 101) when the group changes.
        // AlwaysFailWriter trips that branch on the first paragraph's
        // writeln, before the speaker change is even reached — close
        // enough to exercise the second-paragraph entry; the regression
        // we want to catch is "errors mid-stream don't get swallowed."
        let events = [
            Ok(final_event("a", 0, Some("alice"))),
            Ok(final_event("b", 1, Some("bob"))),
        ];
        let err = render_markdown(events, &mut AlwaysFailWriter).unwrap_err();
        assert!(err.to_string().contains("forced write failure"));
    }

    #[test]
    fn render_markdown_empty_input_writes_nothing() {
        let out = render_md_to_string::<[TranscriptEvent; 0]>([]);
        assert_eq!(out, "");
    }
}
