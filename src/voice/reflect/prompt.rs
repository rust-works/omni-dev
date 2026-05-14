//! Prompt template loading and rendering.
//!
//! The template lives at `src/voice/prompts/reflect.md` and is loaded via
//! `include_str!` so its sha256 fingerprint is stable per build —
//! `prompt_version` returns the first 8 hex chars of that hash and is
//! recorded on every emitted event's `provenance.prompt_version` field,
//! letting reconciliation tell which prompt revision produced which
//! events.

use std::fmt::Write as _;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::voice::events::{ProjectedItem, ProjectedState};
use crate::voice::TranscriptEvent;

/// The reflection prompt template, bundled at compile time.
pub const TEMPLATE: &str = include_str!("../prompts/reflect.md");

/// First eight hex characters of the SHA-256 of [`TEMPLATE`].
///
/// Used as the `prompt_version` field on emitted events.
#[must_use]
pub fn prompt_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let hash = Sha256::digest(TEMPLATE.as_bytes());
        let mut out = String::with_capacity(8);
        for b in &hash[..4] {
            let _ = write!(out, "{b:02x}");
        }
        out
    })
}

/// Renders the prompt with `{{current_state}}` and `{{new_transcript}}` substituted in.
///
/// No escaping — the substitution targets live inside fenced blocks in
/// the template, where YAML cannot prematurely terminate the surrounding
/// document.
#[must_use]
pub fn render(current_state: &str, new_transcript: &str) -> String {
    TEMPLATE
        .replace("{{current_state}}", current_state)
        .replace("{{new_transcript}}", new_transcript)
}

/// Renders a [`ProjectedState`] as the `<current_state>` block body. Skips
/// completed and expired items — the LLM only needs to see the
/// still-actionable working set.
#[must_use]
pub fn format_current_state(state: &ProjectedState) -> String {
    let mut out = String::new();
    let active: Vec<&ProjectedItem> = state
        .items
        .values()
        .filter(|i| !i.completed && i.expired.is_none())
        .collect();

    if !active.is_empty() {
        out.push_str("## Items\n\n");
        for item in &active {
            let class = match item.class {
                crate::voice::events::ItemClass::Todo => "todo",
                crate::voice::events::ItemClass::Research => "research",
                crate::voice::events::ItemClass::Question => "question",
            };
            let _ = writeln!(out, "- [{}] {}: {}", item.id, class, item.text);
        }
        out.push('\n');
    }

    if !state.decisions.is_empty() {
        out.push_str("## Decisions\n\n");
        for d in &state.decisions {
            let _ = writeln!(out, "- [{}] {}", d.id, d.text);
        }
        out.push('\n');
    }

    if !state.notes.is_empty() {
        out.push_str("## Notes\n\n");
        for n in &state.notes {
            let _ = writeln!(out, "- [{}] {}", n.id, n.text);
        }
    }

    if out.is_empty() {
        "(empty)\n".to_string()
    } else {
        out
    }
}

/// Renders the consumed `Final` transcript events as the `<new_transcript>`
/// block body, one event per line in the form `[<event_id>] <text>`.
#[must_use]
pub fn format_new_transcript(finals: &[TranscriptEvent]) -> String {
    let mut out = String::new();
    for event in finals {
        if let TranscriptEvent::Final { event_id, text, .. } = event {
            let _ = writeln!(out, "[{event_id}] {text}");
        }
    }
    if out.is_empty() {
        "(no new transcript)\n".to_string()
    } else {
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::events::{
        ItemClass, Priority, ProjectedDecision, ProjectedItem, ProjectedNote,
    };
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn ulid(n: u128) -> ulid::Ulid {
        ulid::Ulid::from_parts(0, n)
    }

    #[test]
    fn prompt_version_is_eight_hex_chars() {
        let v = prompt_version();
        assert_eq!(v.len(), 8);
        assert!(v.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn prompt_version_is_stable_across_calls() {
        assert_eq!(prompt_version(), prompt_version());
    }

    #[test]
    fn render_substitutes_both_placeholders() {
        let out = render("STATE_BODY", "TRANSCRIPT_BODY");
        assert!(out.contains("STATE_BODY"), "current_state not substituted");
        assert!(
            out.contains("TRANSCRIPT_BODY"),
            "new_transcript not substituted"
        );
        assert!(!out.contains("{{current_state}}"));
        assert!(!out.contains("{{new_transcript}}"));
    }

    #[test]
    fn format_current_state_empty_returns_placeholder() {
        let state = ProjectedState::default();
        let s = format_current_state(&state);
        assert_eq!(s, "(empty)\n");
    }

    #[test]
    fn format_current_state_skips_completed_and_expired() {
        let mut items = BTreeMap::new();
        items.insert(
            ulid(1),
            ProjectedItem {
                id: ulid(1),
                class: ItemClass::Todo,
                text: "active".into(),
                priority: Priority::Normal,
                valid_until: None,
                tags: vec![],
                completed: false,
                expired: None,
            },
        );
        items.insert(
            ulid(2),
            ProjectedItem {
                id: ulid(2),
                class: ItemClass::Todo,
                text: "done".into(),
                priority: Priority::Normal,
                valid_until: None,
                tags: vec![],
                completed: true,
                expired: None,
            },
        );
        items.insert(
            ulid(3),
            ProjectedItem {
                id: ulid(3),
                class: ItemClass::Todo,
                text: "expired".into(),
                priority: Priority::Normal,
                valid_until: None,
                tags: vec![],
                completed: false,
                expired: Some(crate::voice::events::ExpireReason::Retracted),
            },
        );
        let state = ProjectedState {
            items,
            decisions: vec![],
            notes: vec![],
        };
        let s = format_current_state(&state);
        assert!(s.contains("active"));
        assert!(!s.contains("done"));
        assert!(!s.contains("expired"));
    }

    #[test]
    fn format_current_state_renders_all_three_classes() {
        let mut items = BTreeMap::new();
        for (n, class) in [
            (1u128, ItemClass::Todo),
            (2, ItemClass::Research),
            (3, ItemClass::Question),
        ] {
            items.insert(
                ulid(n),
                ProjectedItem {
                    id: ulid(n),
                    class,
                    text: format!("text-{n}"),
                    priority: Priority::Normal,
                    valid_until: None,
                    tags: vec![],
                    completed: false,
                    expired: None,
                },
            );
        }
        let state = ProjectedState {
            items,
            decisions: vec![],
            notes: vec![],
        };
        let s = format_current_state(&state);
        assert!(s.contains("todo: text-1"), "missing todo: {s}");
        assert!(s.contains("research: text-2"), "missing research: {s}");
        assert!(s.contains("question: text-3"), "missing question: {s}");
    }

    #[test]
    fn format_current_state_includes_decisions_and_notes() {
        let state = ProjectedState {
            items: BTreeMap::new(),
            decisions: vec![ProjectedDecision {
                id: ulid(1),
                text: "decided X".into(),
                alternatives: vec![],
            }],
            notes: vec![ProjectedNote {
                id: ulid(2),
                text: "noted Y".into(),
                links: vec![],
                valid_until: None,
            }],
        };
        let s = format_current_state(&state);
        assert!(s.contains("decided X"));
        assert!(s.contains("noted Y"));
        assert!(s.contains("Decisions"));
        assert!(s.contains("Notes"));
    }

    #[test]
    fn format_new_transcript_skips_partials_and_endpoints() {
        let finals = vec![
            TranscriptEvent::Partial {
                text: "ignored".into(),
                start: Duration::ZERO,
                end: Duration::from_millis(50),
                words: None,
                speaker: None,
            },
            TranscriptEvent::Final {
                event_id: ulid(1),
                text: "kept".into(),
                start: Duration::ZERO,
                end: Duration::from_millis(50),
                confidence: 0.9,
                words: None,
                speaker: None,
                revisable: false,
            },
            TranscriptEvent::Endpoint {
                at: Duration::from_secs(1),
                kind: crate::voice::EndpointKind::StreamEnd,
            },
        ];
        let s = format_new_transcript(&finals);
        assert!(s.contains("kept"));
        assert!(!s.contains("ignored"));
        // Endpoint variant has no text body in our format.
    }

    #[test]
    fn format_new_transcript_empty_returns_placeholder() {
        assert_eq!(format_new_transcript(&[]), "(no new transcript)\n");
    }
}
