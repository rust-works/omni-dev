//! The stream-json tracker (Feed 4): the pure state machine behind
//! `omni-dev claude-wrap`, turning Claude's `--output-format stream-json` stdio
//! into [`ObserveRequest`]s for the [`SessionsRegistry`].
//!
//! Unlike the other three feeds this one is **authoritative**. Hooks and the
//! transcript watcher observe a session from the outside and infer what it is
//! doing; the wrapper sits *in* the stream the Claude VS Code extension itself
//! reads, so it sees the exact protocol events — including `can_use_tool`, the
//! permission prompt that never reaches a transcript and is therefore invisible
//! to Feeds 1–3 unless the user has the `Notification` hook installed. See
//! ADR-0057.
//!
//! Both directions matter. A permission request travels **CLI → editor** on the
//! child's stdout as a `control_request`; its resolution travels **editor → CLI**
//! on the child's stdin as a `control_response`. Correlating the two by
//! `request_id` is what makes `waiting_for_permission` exact rather than a guess,
//! so [`StreamTracker::observe_line`] takes the [`Direction`] a line was seen in.
//! A model switch can be the same shape the other way around: `set_model` is a
//! `control_request` an SDK-driven caller can send **to** the CLI, over a
//! *live* process's stdin, to switch models without waiting for the next
//! turn's `system`/`init` line (#1448 follow-up). In practice this rarely
//! fires for the Claude VS Code extension itself: its in-chat `/model` command
//! is a local UI action that never sends `set_model` at all (it mutates local
//! state and persists to the user's settings file instead), and the extension
//! respawns a fresh wrapped process per turn rather than keeping one alive —
//! so for that caller, a switch is still only observed via the next turn's
//! `system`/`init` line, same as every other identity field here.
//!
//! Nothing here does I/O and nothing here retains conversation content: only the
//! message `type`/`subtype`, the identity fields (`session_id`, `cwd`, `model`)
//! and outstanding permission ids are ever read out of a line. Every parse is
//! best-effort — an unparseable or unrecognized line is ignored, never fatal,
//! because the wrapper must fail open (see [`crate::cli::claude_wrap`]).
//!
//! [`SessionsRegistry`]: super::SessionsRegistry

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Deserialize;

use super::{ObserveRequest, SessionEvent, SessionState};

/// Ceiling on simultaneously-outstanding permission requests tracked at once.
///
/// Claude asks about one tool at a time in practice, so this only bounds memory
/// against a malformed or adversarial stream; ids past the cap are dropped,
/// which can only ever make the tracker return to `working` early.
const MAX_PENDING_PERMISSIONS: usize = 64;

/// Which side of the wrapped process's stdio a line was observed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A line the wrapped `claude` wrote to its **stdout** (CLI → editor).
    FromClaude,
    /// A line the editor wrote to the wrapped `claude`'s **stdin** (editor → CLI).
    ToClaude,
}

/// The subset of a stream-json line this tracker reads.
///
/// Deliberately minimal and fully optional: the stream schema is Claude Code's
/// internal protocol, so anything unrecognized must deserialize successfully and
/// be ignored rather than break the feed. Conversation content (`message`,
/// tool inputs, results) is never named here, so it is never even materialized.
#[derive(Debug, Default, Deserialize)]
struct StreamLine {
    /// The message kind: `system`, `assistant`, `user`, `result`, `stream_event`,
    /// `control_request`, `control_response`, …
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// The `system` message's discriminator, notably `init`.
    #[serde(default)]
    subtype: Option<String>,
    /// The Claude session id, carried on most messages.
    #[serde(default)]
    session_id: Option<String>,
    /// The session's working directory, carried on `system`/`init`.
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// The model id, carried on `system`/`init`.
    #[serde(default)]
    model: Option<String>,
    /// A control message's correlation id, when carried at the top level.
    #[serde(default)]
    request_id: Option<String>,
    /// A `control_request`'s body.
    #[serde(default)]
    request: Option<ControlBody>,
    /// A `control_response`'s body.
    #[serde(default)]
    response: Option<ControlBody>,
}

/// The shared shape of a `control_request` / `control_response` body.
#[derive(Debug, Default, Deserialize)]
struct ControlBody {
    /// The control kind, e.g. `can_use_tool`, `initialize`, `hook_callback`,
    /// `set_model`.
    #[serde(default)]
    subtype: Option<String>,
    /// The correlation id, when carried inside the body rather than at the top
    /// level (a `control_response` echoes it here).
    #[serde(default)]
    request_id: Option<String>,
    /// A `set_model` request's target model id. Per Claude Code's own schema
    /// this field is optional and simply **absent** when the editor asks to
    /// reset to the account/session default (a value only the CLI itself can
    /// resolve) — that case is left for the next `system`/`init` line to
    /// settle. The literal string `"default"` is guarded against too, purely
    /// defensively: nothing in the documented protocol sends it, but nothing
    /// rules out a caller that does.
    #[serde(default)]
    model: Option<String>,
}

/// The authoritative session-state machine over one wrapped `claude` process.
///
/// Feed it every line of both stdio directions; it returns an [`ObserveRequest`]
/// exactly when the session's effective state *changes*, so a long turn costs one
/// report at its start and one at its end rather than one per streamed token.
#[derive(Debug)]
pub struct StreamTracker {
    /// The session id, learned from the first line that carries one.
    session_id: Option<String>,
    /// The session's working directory, learned from `system`/`init`.
    cwd: Option<PathBuf>,
    /// The model currently in use: from the most recent `system`/`init` line,
    /// or — sooner — a `set_model` `control_request` the editor sent. Unlike
    /// `session_id`/`cwd`, this is **not** fill-once: Claude Code re-emits
    /// `system`/`init` — model included — at the start of every turn (not just
    /// session start), and a `set_model` request updates it immediately, at
    /// the moment of the switch rather than the next turn (#1448 follow-up).
    model: Option<String>,
    /// The state implied by the most recent content message, before the
    /// permission overlay is applied.
    base: SessionState,
    /// The `request_id`s of permission prompts asked but not yet answered.
    pending: HashSet<String>,
    /// The (state, model) pair most recently returned to the caller, for
    /// change detection. Both dimensions are tracked because they change
    /// independently: an ordinary turn changes state without the model, while
    /// a mid-turn `set_model` changes the model without the state.
    reported: Option<(SessionState, Option<String>)>,
}

impl StreamTracker {
    /// Creates a tracker for one wrapped process, before any line is seen.
    #[must_use]
    pub fn new() -> Self {
        Self {
            session_id: None,
            cwd: None,
            model: None,
            // A `claude` that has started but has not been prompted is sitting at
            // the prompt: idle, not "starting". `Starting` stays the hook feed's
            // (very brief) `SessionStart` state, so both feeds agree that a tab
            // the user opened and has not typed into is not doing work.
            base: SessionState::Idle,
            pending: HashSet::new(),
            reported: None,
        }
    }

    /// The session id, once a line has carried one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Feeds one stdio line and returns a sighting when the effective state
    /// changed as a result.
    ///
    /// Returns `None` for every line that is unparseable, unrecognized, seen
    /// before the session id is known, or that leaves the state unchanged.
    pub fn observe_line(&mut self, direction: Direction, line: &str) -> Option<ObserveRequest> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let parsed: StreamLine = serde_json::from_str(line).ok()?;
        self.absorb_identity(direction, &parsed);
        self.apply(direction, &parsed);
        self.emit_if_changed()
    }

    /// Re-reports the current state, so a session that has been silent for a
    /// while does not age out of the registry on its TTL.
    ///
    /// The wrapper lives exactly as long as the `claude` process does, so this
    /// is real liveness rather than the activity-based approximation the hook and
    /// transcript feeds are limited to.
    #[must_use]
    pub fn keepalive(&self) -> Option<ObserveRequest> {
        self.request(self.state())
    }

    /// Records the identity fields carried on a line: `session_id`/`cwd` are
    /// fill-once (never overwriting a value already learned with a later
    /// absent one), but `model` always takes the latest non-empty value seen,
    /// from either of two shapes — see the field doc on [`Self::model`] for
    /// why.
    fn absorb_identity(&mut self, direction: Direction, parsed: &StreamLine) {
        if self.session_id.is_none() {
            if let Some(id) = parsed.session_id.as_deref() {
                if !id.trim().is_empty() {
                    self.session_id = Some(id.to_string());
                }
            }
        }
        if self.cwd.is_none() {
            self.cwd.clone_from(&parsed.cwd);
        }
        // The top-level `model`, carried on a `system`/`init` line.
        if let Some(model) = parsed.model.as_deref() {
            if !model.trim().is_empty() {
                self.model = Some(model.to_string());
            }
        }
        // A `set_model` control_request, which — unlike every other identity
        // signal here — travels editor → CLI, so it is only ever trusted on
        // that direction (the same rule `close_permission` applies to a
        // permission answer). An unresolved target — the field absent per the
        // documented schema, or (defensively) the literal string "default" —
        // cannot be resolved without the CLI's own account/session settings,
        // so it is left alone rather than guessed; the next `system`/`init`
        // line settles it instead.
        //
        // Note this path is only reachable while a wrapped process is alive:
        // Claude Code's own in-chat `/model` command does not send this
        // control_request at all (it mutates local state and persists to the
        // user's settings file instead), and the VS Code extension respawns a
        // fresh wrapped process per turn rather than keeping one alive across
        // a whole conversation. So in practice, a model switch is still only
        // observed at the start of the *next* turn's `system`/`init` line
        // (handled above) — this block is a correctness improvement for
        // whatever caller does send `set_model` to a live process (e.g. an
        // external SDK-driven consumer), not a guaranteed instant path for
        // this extension's own in-chat command.
        if direction == Direction::ToClaude && parsed.kind.as_deref() == Some("control_request") {
            if let Some(model) = parsed
                .request
                .as_ref()
                .filter(|body| body.subtype.as_deref() == Some("set_model"))
                .and_then(|body| body.model.as_deref())
            {
                if !model.trim().is_empty() && model != "default" {
                    self.model = Some(model.to_string());
                }
            }
        }
    }

    /// Applies a line's state effect: content messages move [`Self::base`],
    /// control messages open and close permission prompts.
    fn apply(&mut self, direction: Direction, parsed: &StreamLine) {
        match parsed.kind.as_deref() {
            // The session announced itself but has not been prompted yet.
            Some("system") if parsed.subtype.as_deref() == Some("init") => {
                self.base = SessionState::Idle;
            }
            // A replayed user prompt, a streamed assistant reply, or a tool
            // result: the turn is running.
            Some("assistant" | "user" | "stream_event") => self.base = SessionState::Working,
            // The turn finished. Also the drift backstop: if the stream ever
            // stops answering a permission request in a shape this tracker
            // recognizes, a completed turn unwedges it rather than pinning the
            // session on `waiting_for_permission` forever.
            Some("result") => {
                self.base = SessionState::Idle;
                self.pending.clear();
            }
            Some("control_request") if direction == Direction::FromClaude => {
                self.open_permission(parsed);
            }
            Some("control_response") if direction == Direction::ToClaude => {
                self.close_permission(parsed);
            }
            _ => {}
        }
    }

    /// Records a `can_use_tool` request as outstanding; every other control
    /// subtype (`initialize`, `hook_callback`, `mcp_message`, …) carries no
    /// state signal and is ignored.
    fn open_permission(&mut self, parsed: &StreamLine) {
        let body = parsed.request.as_ref();
        if body.and_then(|b| b.subtype.as_deref()) != Some("can_use_tool") {
            return;
        }
        let Some(id) = correlation_id(parsed, body) else {
            return;
        };
        if self.pending.len() < MAX_PENDING_PERMISSIONS {
            self.pending.insert(id);
        }
    }

    /// Clears the permission request a `control_response` answers.
    fn close_permission(&mut self, parsed: &StreamLine) {
        let body = parsed.response.as_ref();
        if let Some(id) = correlation_id(parsed, body) {
            self.pending.remove(&id);
        }
    }

    /// The effective state: an outstanding permission prompt outranks whatever
    /// the content messages last implied, so a reply that keeps streaming while
    /// the user is being asked to approve a tool cannot mask the prompt.
    fn state(&self) -> SessionState {
        if self.pending.is_empty() {
            self.base
        } else {
            SessionState::WaitingForPermission
        }
    }

    /// Returns a sighting when the effective state or the model differs from
    /// what was last reported, and records the new pair as reported. Emitting
    /// on a model-only change (no state change) is what makes a `set_model`
    /// control_request reach the daemon immediately rather than waiting for
    /// the next state transition to carry it along.
    fn emit_if_changed(&mut self) -> Option<ObserveRequest> {
        let state = self.state();
        let signature = (state, self.model.clone());
        if self.reported.as_ref() == Some(&signature) {
            return None;
        }
        let request = self.request(state)?;
        self.reported = Some(signature);
        Some(request)
    }

    /// Builds the sighting for `state`, or `None` while the session id is still
    /// unknown (nothing can be keyed without it).
    fn request(&self, state: SessionState) -> Option<ObserveRequest> {
        Some(ObserveRequest {
            session_id: self.session_id.clone()?,
            cwd: self.cwd.clone(),
            transcript_path: None,
            event: SessionEvent::StreamState(state),
            repo: None,
            model: self.model.clone(),
        })
    }
}

impl Default for StreamTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// A control message's correlation id, from the body when present (where a
/// `control_response` echoes it) and otherwise from the top level.
fn correlation_id(parsed: &StreamLine, body: Option<&ControlBody>) -> Option<String> {
    body.and_then(|b| b.request_id.clone())
        .or_else(|| parsed.request_id.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1","cwd":"/w/repo","model":"claude-opus-5","tools":["Read"]}"#;

    fn tracker_after_init() -> StreamTracker {
        let mut tracker = StreamTracker::new();
        let first = tracker
            .observe_line(Direction::FromClaude, INIT)
            .expect("init announces the session");
        assert_eq!(first.session_id, "sess-1");
        tracker
    }

    fn state_of(request: &ObserveRequest) -> SessionState {
        match request.event {
            SessionEvent::StreamState(state) => state,
            other => panic!("expected a stream state, got {other:?}"),
        }
    }

    #[test]
    fn init_announces_the_session_as_idle_with_its_identity() {
        let mut tracker = StreamTracker::new();
        let request = tracker.observe_line(Direction::FromClaude, INIT).unwrap();
        assert_eq!(request.session_id, "sess-1");
        assert_eq!(
            request.cwd.as_deref(),
            Some(std::path::Path::new("/w/repo"))
        );
        assert_eq!(request.model.as_deref(), Some("claude-opus-5"));
        // A started-but-unprompted session sits at the prompt.
        assert_eq!(state_of(&request), SessionState::Idle);
        assert_eq!(tracker.session_id(), Some("sess-1"));
    }

    #[test]
    fn nothing_is_reported_before_a_session_id_is_known() {
        let mut tracker = StreamTracker::new();
        // A content message with no session id moves the state but cannot be keyed.
        assert!(tracker
            .observe_line(Direction::FromClaude, r#"{"type":"assistant"}"#)
            .is_none());
        assert!(tracker.keepalive().is_none());
        // …and the state it moved to is reported as soon as an id arrives.
        let request = tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"assistant","session_id":"sess-1"}"#,
            )
            .unwrap();
        assert_eq!(state_of(&request), SessionState::Working);
    }

    #[test]
    fn a_turn_reports_working_then_idle_once_each() {
        let mut tracker = tracker_after_init();
        let working = tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"user","session_id":"sess-1"}"#,
            )
            .unwrap();
        assert_eq!(state_of(&working), SessionState::Working);
        // Streaming does not re-report: the state has not changed.
        assert!(tracker
            .observe_line(Direction::FromClaude, r#"{"type":"stream_event"}"#)
            .is_none());
        assert!(tracker
            .observe_line(Direction::FromClaude, r#"{"type":"assistant"}"#)
            .is_none());
        let idle = tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"result","subtype":"success"}"#,
            )
            .unwrap();
        assert_eq!(state_of(&idle), SessionState::Idle);
    }

    #[test]
    fn a_set_model_request_updates_and_reports_the_model_immediately() {
        // `set_model` travels editor -> CLI at the exact moment the user
        // switches models — no turn required to observe it (#1448 follow-up).
        let mut tracker = tracker_after_init(); // model = claude-opus-5
        let switched = tracker
            .observe_line(
                Direction::ToClaude,
                r#"{"type":"control_request","request_id":"c1","request":{"subtype":"set_model","model":"claude-sonnet-5"}}"#,
            )
            .unwrap();
        assert_eq!(switched.model.as_deref(), Some("claude-sonnet-5"));
        // The state itself did not change — this session is still idle.
        assert_eq!(state_of(&switched), SessionState::Idle);
    }

    #[test]
    fn a_set_model_with_no_model_field_is_left_for_the_next_init_to_settle() {
        // Per the documented schema, "reset to the account/session default" is
        // signaled by omitting `model` entirely, not by a literal value — that
        // cannot be resolved without the CLI's own settings, so it is not
        // taken at face value...
        let mut tracker = tracker_after_init(); // model = claude-opus-5
        assert!(tracker
            .observe_line(
                Direction::ToClaude,
                r#"{"type":"control_request","request_id":"c1","request":{"subtype":"set_model"}}"#,
            )
            .is_none());
        // ...and the model is unchanged until the next turn's init resolves it.
        let switched_init = r#"{"type":"system","subtype":"init","session_id":"sess-1","cwd":"/w/repo","model":"claude-haiku-4-5","tools":["Read"]}"#;
        let resolved = tracker
            .observe_line(Direction::FromClaude, switched_init)
            .unwrap();
        assert_eq!(resolved.model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn a_set_model_literal_default_string_is_also_left_unresolved() {
        // Defensive only: nothing in the documented schema sends the literal
        // string "default" (the field is simply omitted instead — see the
        // previous test), but a hypothetical caller that did should not have
        // that string taken as a real model id either.
        let mut tracker = tracker_after_init(); // model = claude-opus-5
        assert!(tracker
            .observe_line(
                Direction::ToClaude,
                r#"{"type":"control_request","request_id":"c1","request":{"subtype":"set_model","model":"default"}}"#,
            )
            .is_none());
        assert_eq!(
            tracker.keepalive().unwrap().model.as_deref(),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn a_set_model_request_is_only_honored_from_the_editor() {
        // The same line seen on the wrong direction (as if Claude itself sent
        // it) is not trusted, mirroring the permission-response direction rule.
        let mut tracker = tracker_after_init(); // model = claude-opus-5
        assert!(tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"control_request","request_id":"c1","request":{"subtype":"set_model","model":"claude-sonnet-5"}}"#,
            )
            .is_none());
        assert_eq!(
            tracker.keepalive().unwrap().model.as_deref(),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn a_later_init_confirming_the_already_set_model_does_not_re_emit() {
        // Claude Code re-announces itself via `system`/`init` at the start of
        // every turn, not just session start. Once `set_model` has already
        // moved the tracker to the new model, that turn's init line — which
        // confirms the same model — is a no-op rather than a redundant report.
        let mut tracker = tracker_after_init(); // model = claude-opus-5
        tracker.observe_line(
            Direction::ToClaude,
            r#"{"type":"control_request","request_id":"c1","request":{"subtype":"set_model","model":"claude-sonnet-5"}}"#,
        );
        let confirming_init = r#"{"type":"system","subtype":"init","session_id":"sess-1","cwd":"/w/repo","model":"claude-sonnet-5","tools":["Read"]}"#;
        assert!(tracker
            .observe_line(Direction::FromClaude, confirming_init)
            .is_none());
    }

    #[test]
    fn a_permission_prompt_reports_waiting_until_it_is_answered() {
        let mut tracker = tracker_after_init();
        tracker.observe_line(Direction::FromClaude, r#"{"type":"assistant"}"#);
        let waiting = tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash"}}"#,
            )
            .unwrap();
        assert_eq!(state_of(&waiting), SessionState::WaitingForPermission);
        // Content still streaming while the user is asked must not mask the prompt.
        assert!(tracker
            .observe_line(Direction::FromClaude, r#"{"type":"assistant"}"#)
            .is_none());
        // The answer arrives on the *other* direction, echoing the id in its body.
        let resumed = tracker
            .observe_line(
                Direction::ToClaude,
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1"}}"#,
            )
            .unwrap();
        assert_eq!(state_of(&resumed), SessionState::Working);
    }

    #[test]
    fn a_permission_response_is_only_honored_from_the_editor() {
        let mut tracker = tracker_after_init();
        tracker.observe_line(
            Direction::FromClaude,
            r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool"}}"#,
        );
        // The same line seen on the wrong direction is not the answer.
        assert!(tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"control_response","response":{"request_id":"req-1"}}"#,
            )
            .is_none());
        assert_eq!(
            state_of(&tracker.keepalive().unwrap()),
            SessionState::WaitingForPermission
        );
    }

    #[test]
    fn other_control_subtypes_carry_no_state_signal() {
        let mut tracker = tracker_after_init();
        for subtype in ["initialize", "hook_callback", "mcp_message", "interrupt"] {
            let line = format!(
                r#"{{"type":"control_request","request_id":"c","request":{{"subtype":"{subtype}"}}}}"#
            );
            assert!(tracker.observe_line(Direction::FromClaude, &line).is_none());
        }
        assert_eq!(state_of(&tracker.keepalive().unwrap()), SessionState::Idle);
    }

    #[test]
    fn a_finished_turn_unwedges_a_stranded_permission() {
        let mut tracker = tracker_after_init();
        tracker.observe_line(
            Direction::FromClaude,
            r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool"}}"#,
        );
        // No matching response ever arrives (protocol drift); `result` still ends
        // the turn rather than pinning the session on `waiting_for_permission`.
        let idle = tracker
            .observe_line(Direction::FromClaude, r#"{"type":"result"}"#)
            .unwrap();
        assert_eq!(state_of(&idle), SessionState::Idle);
    }

    #[test]
    fn outstanding_permissions_are_capped() {
        let mut tracker = tracker_after_init();
        for i in 0..(MAX_PENDING_PERMISSIONS + 10) {
            let line = format!(
                r#"{{"type":"control_request","request_id":"req-{i}","request":{{"subtype":"can_use_tool"}}}}"#
            );
            tracker.observe_line(Direction::FromClaude, &line);
        }
        assert_eq!(tracker.pending.len(), MAX_PENDING_PERMISSIONS);
    }

    #[test]
    fn a_blank_session_id_is_not_taken_as_identity() {
        let mut tracker = StreamTracker::new();
        assert!(tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"assistant","session_id":"   "}"#,
            )
            .is_none());
        assert_eq!(tracker.session_id(), None);
        // …and a real id later still lands.
        tracker.observe_line(Direction::FromClaude, INIT);
        assert_eq!(tracker.session_id(), Some("sess-1"));
    }

    #[test]
    fn control_messages_with_no_correlation_id_are_ignored() {
        let mut tracker = tracker_after_init();
        // A permission request that cannot be correlated is not tracked, rather
        // than pinning the session on a prompt nothing can ever answer.
        assert!(tracker
            .observe_line(
                Direction::FromClaude,
                r#"{"type":"control_request","request":{"subtype":"can_use_tool"}}"#,
            )
            .is_none());
        assert_eq!(state_of(&tracker.keepalive().unwrap()), SessionState::Idle);
        // Likewise an answer that names no request clears nothing.
        tracker.observe_line(
            Direction::FromClaude,
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool"}}"#,
        );
        assert!(tracker
            .observe_line(Direction::ToClaude, r#"{"type":"control_response"}"#)
            .is_none());
        assert_eq!(
            state_of(&tracker.keepalive().unwrap()),
            SessionState::WaitingForPermission
        );
    }

    #[test]
    fn default_matches_a_fresh_tracker() {
        let tracker = StreamTracker::default();
        assert_eq!(tracker.session_id(), None);
        assert!(tracker.keepalive().is_none());
    }

    #[test]
    fn unparseable_and_unknown_lines_are_ignored() {
        let mut tracker = tracker_after_init();
        for line in [
            "",
            "   ",
            "not json at all",
            "{",
            "[]",
            r#"{"type":"nonsense"}"#,
            r#"{"no_type":true}"#,
        ] {
            assert!(tracker.observe_line(Direction::FromClaude, line).is_none());
        }
        assert_eq!(state_of(&tracker.keepalive().unwrap()), SessionState::Idle);
    }

    #[test]
    fn keepalive_re_reports_the_current_state_without_a_change() {
        let mut tracker = tracker_after_init();
        tracker.observe_line(Direction::FromClaude, r#"{"type":"assistant"}"#);
        let first = tracker.keepalive().unwrap();
        let second = tracker.keepalive().unwrap();
        assert_eq!(state_of(&first), SessionState::Working);
        assert_eq!(state_of(&second), SessionState::Working);
        assert_eq!(first.session_id, "sess-1");
    }
}
