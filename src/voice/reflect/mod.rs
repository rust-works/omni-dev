//! `voice reflect` — transcript-to-events Claude consumer.
//!
//! Consumes `TranscriptEvent::Final` events from a `transcript.jsonl`
//! source (file path, stdin, or session directory), calls Claude via the
//! existing [`AiClient`], parses the YAML response into [`Event`]s, and
//! appends them to `events.jsonl` (or stdout, for one-shot mode).
//!
//! Per #799: this is step 1 of the build order — text-in / events-out,
//! no audio code. See [the umbrella issue](https://github.com/rust-works/omni-dev/issues/799)
//! for the load-bearing event schema and the rationale for event-sourced
//! reflection.

pub mod prompt;
pub mod validate;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::warn;

use crate::claude::ai::AiClient;
use crate::voice::clock::Clock;
use crate::voice::det::UlidRng;
use crate::voice::events::{
    Event, EventKind, ItemId, Provenance, ReflectionError, ReflectionId, TranscriptSpan,
};
use crate::voice::session::{self, Session};
use crate::voice::{EventId, TranscriptEvent};

/// Where to read the transcript from.
#[derive(Debug, Clone)]
pub enum TranscriptSource {
    /// Read from a JSONL file at this path.
    Path(PathBuf),
    /// Read from standard input.
    Stdin,
    /// Open or create the named session and read its `transcript.jsonl`
    /// (incrementally, after `meta.last_reflected_event_id`).
    Session(String),
}

/// Driver options. The trait fields ([`UlidRng`], [`Clock`], [`AiClient`])
/// are injected so tests can pin them with deterministic implementations.
pub struct ReflectOptions {
    /// Where to read the input transcript from.
    pub source: TranscriptSource,
    /// ULID source for `event_id` / `reflection_id` generation.
    pub ulid_rng: Box<dyn UlidRng>,
    /// Wall-clock source for `ts` fields.
    pub clock: Box<dyn Clock>,
    /// AI client to invoke for the reflection prompt.
    pub ai: Box<dyn AiClient>,
    /// Override the session root directory (test hook). When `None` and
    /// `source = Session(_)`, the standard `~/.omni-dev/voice/` root
    /// (or `OMNI_DEV_VOICE_ROOT`) is used.
    pub session_root_override: Option<PathBuf>,
}

/// System prompt — short, fixed. All schema and behaviour rules live in
/// the user prompt (`src/voice/prompts/reflect.md`) so the
/// `prompt_version` hash captures the full contract.
const SYSTEM_PROMPT: &str = "You convert a voice transcript into structured reflection \
                              events. Follow the format and rules in the user prompt \
                              exactly. Emit ONLY the YAML document — no commentary, no \
                              code fences.";

/// Drives one reflection invocation end-to-end.
///
/// Writes resulting JSONL events to `stdout` for non-session sources,
/// or appends them to the session's `events.jsonl` and updates
/// `meta.last_reflected_event_id` for session-backed runs.
pub async fn run_reflect<W: Write>(opts: ReflectOptions, stdout: &mut W) -> Result<()> {
    let ReflectOptions {
        source,
        mut ulid_rng,
        clock,
        ai,
        session_root_override,
    } = opts;

    // Resolve transcript and existing state.
    let (finals, session, existing_ids) = resolve_input(&source, session_root_override.as_deref())?;

    let Some(span) = compute_span(&finals) else {
        // No Finals consumed — nothing to reflect on. Quiet exit.
        return Ok(());
    };

    // Build the prompt.
    let current_state_body = match &session {
        Some(sess) => {
            let prior = sess.read_events()?;
            let projected = crate::voice::events::project(prior);
            prompt::format_current_state(&projected)
        }
        None => prompt::format_current_state(&crate::voice::events::ProjectedState::default()),
    };
    let new_transcript_body = prompt::format_new_transcript(&finals);
    let user_prompt = prompt::render(&current_state_body, &new_transcript_body);

    // Invoke the AI and time it.
    let reflection_id = ReflectionId::Ulid(ulid_rng.next_ulid());
    let started = Instant::now();
    let ai_response = ai.send_request(SYSTEM_PROMPT, &user_prompt).await;
    let latency_ms = started.elapsed().as_millis();
    let model = ai.get_metadata().model;
    let prompt_version = prompt::prompt_version().to_string();

    let raw_response = match ai_response {
        Ok(s) => s,
        Err(e) => {
            // The AI call itself failed (subprocess crash, timeout, …).
            // Surface it as a reflection.error so the operator can audit.
            let err_event = mint_error_event(
                ulid_rng.as_mut(),
                clock.as_ref(),
                &reflection_id,
                &span,
                &model,
                &prompt_version,
                ReflectionError {
                    raw_output: String::new(),
                    error: format!("AI invocation failed: {e}"),
                },
            );
            return emit_events(
                &session,
                stdout,
                &[err_event],
                /*new_marker*/ Some(span.end_event_id),
                /*reflection_id*/ &reflection_id,
                &model,
                latency_ms,
                /*status*/ "error",
            );
        }
    };

    // Validate; on failure, emit a single reflection.error event.
    let events = match validate::parse_and_validate(&raw_response, &existing_ids) {
        Ok(kinds) => kinds
            .into_iter()
            .map(|kind| {
                build_event(
                    ulid_rng.as_mut(),
                    clock.as_ref(),
                    &reflection_id,
                    &span,
                    &model,
                    &prompt_version,
                    kind,
                )
            })
            .collect::<Vec<_>>(),
        Err(verr) => vec![mint_error_event(
            ulid_rng.as_mut(),
            clock.as_ref(),
            &reflection_id,
            &span,
            &model,
            &prompt_version,
            ReflectionError {
                raw_output: verr.raw_output,
                error: verr.error,
            },
        )],
    };

    let status = if events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ReflectionError(_)))
    {
        "error"
    } else {
        "ok"
    };

    emit_events(
        &session,
        stdout,
        &events,
        Some(span.end_event_id),
        &reflection_id,
        &model,
        latency_ms,
        status,
    )
}

fn resolve_input(
    source: &TranscriptSource,
    session_root_override: Option<&Path>,
) -> Result<(Vec<TranscriptEvent>, Option<Session>, HashSet<ItemId>)> {
    match source {
        TranscriptSource::Path(p) if p.as_os_str() == "-" => {
            let finals = read_finals_from_stdin()?;
            Ok((finals, None, HashSet::new()))
        }
        TranscriptSource::Path(p) => {
            let finals = session::read_transcript_finals_after(p, None)?;
            Ok((finals, None, HashSet::new()))
        }
        TranscriptSource::Stdin => {
            let finals = read_finals_from_stdin()?;
            Ok((finals, None, HashSet::new()))
        }
        TranscriptSource::Session(id) => {
            let sess = match session_root_override {
                Some(root) => session::open_or_create_under(root, id)?,
                None => session::open_or_create(id)?,
            };
            let finals = sess.read_transcript_finals_after()?;
            let existing_state = crate::voice::events::project(sess.read_events()?);
            let existing_ids: HashSet<ItemId> = existing_state.items.keys().copied().collect();
            Ok((finals, Some(sess), existing_ids))
        }
    }
}

fn read_finals_from_stdin() -> Result<Vec<TranscriptEvent>> {
    parse_finals_from_reader(&mut std::io::stdin(), "stdin")
}

/// Reads all `TranscriptEvent`s from `reader`, returning only the `Final`
/// variants. Split out from [`read_finals_from_stdin`] so unit tests can
/// drive it with a `&[u8]` rather than needing a real stdin pipe.
fn parse_finals_from_reader<R: Read>(
    reader: &mut R,
    source_label: &str,
) -> Result<Vec<TranscriptEvent>> {
    let mut body = String::new();
    reader
        .read_to_string(&mut body)
        .with_context(|| format!("reading transcript from {source_label}"))?;
    let mut events = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: TranscriptEvent = serde_json::from_str(line)
            .with_context(|| format!("parsing {source_label} transcript line {}", idx + 1))?;
        if matches!(event, TranscriptEvent::Final { .. }) {
            events.push(event);
        }
    }
    Ok(events)
}

fn compute_span(finals: &[TranscriptEvent]) -> Option<TranscriptSpan> {
    let first = finals.iter().find_map(|e| match e {
        TranscriptEvent::Final { event_id, .. } => Some(*event_id),
        _ => None,
    })?;
    let last = finals.iter().rev().find_map(|e| match e {
        TranscriptEvent::Final { event_id, .. } => Some(*event_id),
        _ => None,
    })?;
    Some(TranscriptSpan {
        start_event_id: first,
        end_event_id: last,
    })
}

fn build_event(
    rng: &mut dyn UlidRng,
    clock: &dyn Clock,
    reflection_id: &ReflectionId,
    span: &TranscriptSpan,
    model: &str,
    prompt_version: &str,
    kind: EventKind,
) -> Event {
    Event {
        event_id: rng.next_ulid(),
        ts: clock.now(),
        reflection_id: reflection_id.clone(),
        provenance: Provenance {
            transcript_span: span.clone(),
            model: Some(model.to_string()),
            prompt_version: Some(prompt_version.to_string()),
        },
        kind: rewrite_kind_with_omitted_optionals(kind),
    }
}

fn mint_error_event(
    rng: &mut dyn UlidRng,
    clock: &dyn Clock,
    reflection_id: &ReflectionId,
    span: &TranscriptSpan,
    model: &str,
    prompt_version: &str,
    err: ReflectionError,
) -> Event {
    build_event(
        rng,
        clock,
        reflection_id,
        span,
        model,
        prompt_version,
        EventKind::ReflectionError(err),
    )
}

/// No-op pass-through today.
///
/// Hook point for future normalisation (e.g. trimming whitespace in
/// user-supplied text). Kept centralised so any later sanitisation
/// lives in one place.
fn rewrite_kind_with_omitted_optionals(kind: EventKind) -> EventKind {
    kind
}

#[allow(clippy::too_many_arguments)]
fn emit_events<W: Write>(
    session: &Option<Session>,
    stdout: &mut W,
    events: &[Event],
    new_marker: Option<EventId>,
    reflection_id: &ReflectionId,
    model: &str,
    latency_ms: u128,
    status: &str,
) -> Result<()> {
    if let Some(sess) = session {
        sess.append_events(events)?;
        if let Some(marker) = new_marker {
            // Clone is cheap; we need a mutable session to update meta.
            let mut sess_mut = sess.clone();
            sess_mut.set_last_reflected(marker)?;
        }
        let refl_id_str = match reflection_id {
            ReflectionId::Ulid(u) => u.to_string(),
            ReflectionId::Review => "review".to_string(),
        };
        let line = format!(
            "{ts} {refl_id_str} model={model} cost_usd=unknown latency_ms={latency_ms} events={n} status={status}",
            ts = chrono::Utc::now().to_rfc3339(),
            n = events.len(),
        );
        sess.append_log(&line)?;
    } else {
        for event in events {
            serde_json::to_writer(&mut *stdout, event)
                .context("serialising reflection event to stdout")?;
            stdout
                .write_all(b"\n")
                .context("writing newline to stdout")?;
        }
        stdout
            .flush()
            .context("flushing reflection events to stdout")?;
        // No log line for non-session mode — the events.jsonl on stdout
        // is the audit trail.
        if status == "error" {
            warn!(
                model = %model,
                latency_ms,
                "reflect completed with errors (see emitted reflection.error event)"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude::test_utils::ConfigurableMockAiClient;
    use crate::voice::clock::FixedClock;
    use crate::voice::det::CountingUlidRng;
    use crate::voice::events::ItemClass;
    use std::time::Duration;
    use tempfile::TempDir;

    fn make_final(event_id: u128, text: &str) -> TranscriptEvent {
        TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, event_id),
            text: text.to_string(),
            start: Duration::ZERO,
            end: Duration::from_millis(500),
            confidence: 0.95,
            words: None,
            speaker: None,
            revisable: false,
        }
    }

    fn write_transcript(tmp: &TempDir, finals: &[TranscriptEvent]) -> PathBuf {
        let path = tmp.path().join("transcript.jsonl");
        let mut body = String::new();
        for e in finals {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn fixed_opts(source: TranscriptSource, ai_responses: Vec<Result<String>>) -> ReflectOptions {
        ReflectOptions {
            source,
            ulid_rng: Box::new(CountingUlidRng::new()),
            clock: Box::new(FixedClock::from_rfc3339("2026-01-01T00:00:00Z")),
            ai: Box::new(ConfigurableMockAiClient::new(ai_responses)),
            session_root_override: None,
        }
    }

    #[tokio::test]
    async fn path_source_emits_events_to_stdout() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(&tmp, &[make_final(1, "wire it up")]);
        let canned = r"events:
  - event_type: item.create
    payload:
      item_id: 00000000000000000000000007
      class: todo
      text: wire it up
";
        let opts = fixed_opts(
            TranscriptSource::Path(transcript),
            vec![Ok(canned.to_string())],
        );
        let mut out: Vec<u8> = Vec::new();
        run_reflect(opts, &mut out).await.unwrap();
        let body = String::from_utf8(out).unwrap();
        assert_eq!(body.lines().count(), 1, "expected exactly one event line");
        let event: Event = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert!(matches!(event.kind, EventKind::ItemCreate(_)));
    }

    #[tokio::test]
    async fn empty_transcript_exits_quietly() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(&tmp, &[]);
        let opts = fixed_opts(
            TranscriptSource::Path(transcript),
            vec![/* AI never called */],
        );
        let mut out: Vec<u8> = Vec::new();
        run_reflect(opts, &mut out).await.unwrap();
        assert!(out.is_empty(), "no events expected when no Finals consumed");
    }

    #[tokio::test]
    async fn malformed_response_yields_reflection_error_event() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(&tmp, &[make_final(1, "talk")]);
        let canned = "this is not yaml: - definitely not";
        let opts = fixed_opts(
            TranscriptSource::Path(transcript),
            vec![Ok(canned.to_string())],
        );
        let mut out: Vec<u8> = Vec::new();
        run_reflect(opts, &mut out).await.unwrap();
        let body = String::from_utf8(out).unwrap();
        assert_eq!(body.lines().count(), 1);
        let event: Event = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        match &event.kind {
            EventKind::ReflectionError(e) => {
                assert!(e.raw_output.contains("not yaml"));
                assert!(e.error.contains("YAML parse failure"));
            }
            other => panic!("expected ReflectionError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ai_invocation_failure_yields_reflection_error_event() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(&tmp, &[make_final(1, "talk")]);
        let opts = fixed_opts(
            TranscriptSource::Path(transcript),
            vec![Err(anyhow::anyhow!("simulated subprocess crash"))],
        );
        let mut out: Vec<u8> = Vec::new();
        run_reflect(opts, &mut out).await.unwrap();
        let body = String::from_utf8(out).unwrap();
        assert_eq!(body.lines().count(), 1);
        let event: Event = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        match &event.kind {
            EventKind::ReflectionError(e) => {
                assert!(e.error.contains("simulated subprocess crash"));
            }
            other => panic!("expected ReflectionError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_source_appends_to_events_jsonl_and_advances_marker() {
        let tmp = TempDir::new().unwrap();
        let voice_root = tmp.path().join("voice-root");
        std::fs::create_dir_all(&voice_root).unwrap();

        // Pre-populate the session with a transcript.
        let sess = session::open_or_create_under(&voice_root, "s1").unwrap();
        std::fs::write(
            &sess.paths.transcript,
            serde_json::to_string(&make_final(1, "first")).unwrap() + "\n",
        )
        .unwrap();

        let canned = r"events:
  - event_type: item.create
    payload:
      item_id: 00000000000000000000000007
      class: todo
      text: first
";
        let mut opts = fixed_opts(
            TranscriptSource::Session("s1".to_string()),
            vec![Ok(canned.to_string())],
        );
        opts.session_root_override = Some(voice_root.clone());
        let mut out: Vec<u8> = Vec::new();
        run_reflect(opts, &mut out).await.unwrap();

        assert!(out.is_empty(), "session writes go to disk, not stdout");
        let appended = std::fs::read_to_string(&sess.paths.events).unwrap();
        assert_eq!(appended.lines().count(), 1);
        let reopened = session::open_or_create_under(&voice_root, "s1").unwrap();
        assert_eq!(
            reopened.meta.last_reflected_event_id,
            Some(ulid::Ulid::from_parts(0, 1))
        );
        let log = std::fs::read_to_string(&sess.paths.log).unwrap();
        assert!(log.contains("status=ok"), "log line missing: {log}");
        assert!(log.contains("events=1"));
    }

    #[tokio::test]
    async fn session_second_reflection_skips_already_consumed_finals() {
        let tmp = TempDir::new().unwrap();
        let voice_root = tmp.path().join("voice-root");
        let sess = session::open_or_create_under(&voice_root, "s1").unwrap();

        // First reflection consumes one final.
        std::fs::write(
            &sess.paths.transcript,
            serde_json::to_string(&make_final(1, "first")).unwrap() + "\n",
        )
        .unwrap();
        let canned1 = r"events:
  - event_type: item.create
    payload:
      item_id: 00000000000000000000000007
      class: todo
      text: first
";
        let mut opts1 = fixed_opts(
            TranscriptSource::Session("s1".to_string()),
            vec![Ok(canned1.to_string())],
        );
        opts1.session_root_override = Some(voice_root.clone());
        let mut sink: Vec<u8> = Vec::new();
        run_reflect(opts1, &mut sink).await.unwrap();

        // A second final is appended (as if the user dictated more).
        use std::io::Write as _;
        let mut transcript_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&sess.paths.transcript)
            .unwrap();
        writeln!(
            transcript_file,
            "{}",
            serde_json::to_string(&make_final(2, "second")).unwrap()
        )
        .unwrap();
        drop(transcript_file);

        // Second reflection should see only the "second" final because
        // meta.last_reflected_event_id now points at ulid 1.
        let canned2 = r"events:
  - event_type: item.create
    payload:
      item_id: 00000000000000000000000008
      class: todo
      text: second
";
        let ai = ConfigurableMockAiClient::new(vec![Ok(canned2.to_string())]);
        let prompts = ai.prompt_handle();
        let opts2 = ReflectOptions {
            source: TranscriptSource::Session("s1".to_string()),
            ulid_rng: Box::new(CountingUlidRng::new()),
            clock: Box::new(FixedClock::from_rfc3339("2026-01-01T00:00:00Z")),
            ai: Box::new(ai),
            session_root_override: Some(voice_root.clone()),
        };
        run_reflect(opts2, &mut sink).await.unwrap();

        let prompts = prompts.prompts();
        assert_eq!(prompts.len(), 1);
        let (_sys, user) = &prompts[0];
        assert!(
            user.contains("second"),
            "second prompt should include 'second'"
        );
        assert!(
            !user.contains("] first"),
            "second prompt should NOT include the already-consumed 'first' transcript line, got: {user}"
        );
    }

    #[tokio::test]
    async fn same_seed_twice_produces_byte_equal_output() {
        let tmp = TempDir::new().unwrap();
        let transcript_path = write_transcript(&tmp, &[make_final(1, "wire it up")]);
        let canned = r"events:
  - event_type: item.create
    payload:
      item_id: 00000000000000000000000007
      class: todo
      text: wire it up
";

        let mut out1: Vec<u8> = Vec::new();
        let opts1 = fixed_opts(
            TranscriptSource::Path(transcript_path.clone()),
            vec![Ok(canned.to_string())],
        );
        run_reflect(opts1, &mut out1).await.unwrap();

        let mut out2: Vec<u8> = Vec::new();
        let opts2 = fixed_opts(
            TranscriptSource::Path(transcript_path),
            vec![Ok(canned.to_string())],
        );
        run_reflect(opts2, &mut out2).await.unwrap();

        assert_eq!(
            out1, out2,
            "deterministic seeds should produce identical output"
        );
    }

    #[test]
    fn item_class_is_used_in_test() {
        // Pin imports so the compiler doesn't warn about ItemClass being
        // imported but unused — it's used in `format_current_state`'s
        // pattern matching, which we don't otherwise exercise here.
        let _ = ItemClass::Todo;
    }

    #[test]
    fn parse_finals_from_reader_filters_partials_and_endpoints() {
        let body = format!(
            "{}\n{}\n{}\n\n",
            serde_json::to_string(&TranscriptEvent::Partial {
                text: "ignored".into(),
                start: Duration::ZERO,
                end: Duration::from_millis(50),
                words: None,
                speaker: None,
            })
            .unwrap(),
            serde_json::to_string(&make_final(1, "kept")).unwrap(),
            serde_json::to_string(&TranscriptEvent::Endpoint {
                at: Duration::from_secs(1),
                kind: crate::voice::EndpointKind::StreamEnd,
            })
            .unwrap(),
        );
        let mut bytes = body.as_bytes();
        let finals = super::parse_finals_from_reader(&mut bytes, "test-source").unwrap();
        assert_eq!(finals.len(), 1, "only the Final should be kept");
        match &finals[0] {
            TranscriptEvent::Final { text, .. } => assert_eq!(text, "kept"),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn parse_finals_from_reader_skips_blank_lines() {
        let body = format!(
            "\n  \n{}\n\n",
            serde_json::to_string(&make_final(1, "only")).unwrap()
        );
        let mut bytes = body.as_bytes();
        let finals = super::parse_finals_from_reader(&mut bytes, "test-source").unwrap();
        assert_eq!(finals.len(), 1);
    }

    #[test]
    fn parse_finals_from_reader_reports_parse_failure_with_line_number() {
        let body = format!(
            "{}\nnot json\n",
            serde_json::to_string(&make_final(1, "ok")).unwrap()
        );
        let mut bytes = body.as_bytes();
        let err = super::parse_finals_from_reader(&mut bytes, "test-source").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("test-source") && msg.contains("line 2"),
            "error should point at line 2: {msg}"
        );
    }
}
