//! Parses and validates the LLM's YAML reflection response.
//!
//! Two-stage: (1) `serde_yaml::from_str` into an `LlmEnvelope` of
//! [`EventKind`]s, then (2) per-variant semantic checks against the
//! existing item IDs in the current session state. Failure returns a
//! [`ValidationError`] that carries the raw output for the caller to
//! attach to a `reflection.error` event.

use std::collections::HashSet;
use std::hash::BuildHasher;

use serde::Deserialize;

use crate::voice::events::{EventKind, ExpireReason, ItemId};

/// What the LLM is expected to emit at the top level — a single YAML
/// document with one key, `events:`, holding the discriminated event
/// list.
#[derive(Debug, Deserialize)]
struct LlmEnvelope {
    events: Vec<EventKind>,
}

/// A parse failure or a semantic check failure.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// One-line human-readable description of what failed.
    pub error: String,
    /// The original LLM output, included in the resulting
    /// `reflection.error` event so an operator can debug.
    pub raw_output: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for ValidationError {}

/// Parses `raw_yaml` and validates each event against the v1 rules.
///
/// `existing_ids` is the set of item IDs already known from the session's
/// projected state. Items minted earlier in the same batch are also
/// allowed as references (the LLM may create-then-update / create-then-
/// complete inside one response).
///
/// On success: returns the validated list of [`EventKind`]s in document
/// order. On failure: returns a [`ValidationError`] carrying `raw_yaml`
/// in its `raw_output` field.
pub fn parse_and_validate<S: BuildHasher>(
    raw_yaml: &str,
    existing_ids: &HashSet<ItemId, S>,
) -> Result<Vec<EventKind>, ValidationError> {
    let envelope: LlmEnvelope = serde_yaml::from_str(raw_yaml).map_err(|e| ValidationError {
        error: format!("YAML parse failure: {e}"),
        raw_output: raw_yaml.to_string(),
    })?;

    let mut known: HashSet<ItemId> = existing_ids.iter().copied().collect();
    let mut seen_new_ids: HashSet<ItemId> = HashSet::new();

    for (idx, kind) in envelope.events.iter().enumerate() {
        if let Err(error) = check_event(kind, &known, &mut seen_new_ids) {
            return Err(ValidationError {
                error: format!("event[{idx}] ({}): {error}", event_name(kind)),
                raw_output: raw_yaml.to_string(),
            });
        }
        // After a successful create, the newly-minted ID becomes
        // referenceable by later events in the same batch.
        if let EventKind::ItemCreate(c) = kind {
            known.insert(c.item_id);
        }
    }

    Ok(envelope.events)
}

fn event_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::ItemCreate(_) => "item.create",
        EventKind::ItemUpdate(_) => "item.update",
        EventKind::ItemExpire(_) => "item.expire",
        EventKind::ItemComplete(_) => "item.complete",
        EventKind::DecisionRecord(_) => "decision.record",
        EventKind::ResearchNote(_) => "research.note",
        EventKind::ReflectionError(_) => "reflection.error",
    }
}

fn check_event(
    kind: &EventKind,
    known: &HashSet<ItemId>,
    seen_new: &mut HashSet<ItemId>,
) -> Result<(), String> {
    match kind {
        EventKind::ItemCreate(c) => {
            if !seen_new.insert(c.item_id) {
                return Err(format!(
                    "duplicate item_id {} minted in this batch",
                    c.item_id
                ));
            }
            if known.contains(&c.item_id) {
                return Err(format!(
                    "item_id {} collides with an existing item from current_state",
                    c.item_id
                ));
            }
            Ok(())
        }
        EventKind::ItemUpdate(u) => require_known(known, u.item_id, "item.update"),
        EventKind::ItemExpire(e) => {
            require_known(known, e.item_id, "item.expire")?;
            if matches!(e.reason, ExpireReason::Ttl) {
                return Err("reason: ttl is reserved for `voice review`; \
                            reflect must use `retracted` or `superseded`"
                    .to_string());
            }
            let is_superseded = matches!(e.reason, ExpireReason::Superseded);
            if is_superseded && e.superseded_by.is_none() {
                return Err("reason: superseded requires superseded_by".to_string());
            }
            if !is_superseded && e.superseded_by.is_some() {
                return Err(format!(
                    "superseded_by is only valid when reason == superseded \
                     (got reason: {:?})",
                    e.reason
                ));
            }
            Ok(())
        }
        EventKind::ItemComplete(c) => require_known(known, c.item_id, "item.complete"),
        EventKind::DecisionRecord(_)
        | EventKind::ResearchNote(_)
        | EventKind::ReflectionError(_) => Ok(()),
    }
}

fn require_known(known: &HashSet<ItemId>, id: ItemId, what: &str) -> Result<(), String> {
    if known.contains(&id) {
        Ok(())
    } else {
        Err(format!(
            "{what} references unknown item_id {id} (not in current_state and not minted earlier in this batch)"
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ulid(n: u128) -> ItemId {
        ulid::Ulid::from_parts(0, n)
    }

    fn no_existing() -> HashSet<ItemId> {
        HashSet::new()
    }

    #[test]
    fn empty_events_list_is_valid() {
        let yaml = "events: []";
        let events = parse_and_validate(yaml, &no_existing()).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn item_create_is_accepted_and_minted_id_is_referenceable() {
        let new_id = ulid(1);
        let yaml = format!(
            "events:\n  - event_type: item.create\n    payload:\n      item_id: {new_id}\n      class: todo\n      text: alpha\n  - event_type: item.update\n    payload:\n      item_id: {new_id}\n      priority: high\n"
        );
        let events = parse_and_validate(&yaml, &no_existing()).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn item_update_unknown_id_errors() {
        let yaml = format!(
            "events:\n  - event_type: item.update\n    payload:\n      item_id: {}\n      text: changed\n",
            ulid(99)
        );
        let err = parse_and_validate(&yaml, &no_existing()).unwrap_err();
        assert!(err.error.contains("unknown item_id"), "got: {}", err.error);
    }

    #[test]
    fn item_complete_unknown_id_errors() {
        let yaml = format!(
            "events:\n  - event_type: item.complete\n    payload:\n      item_id: {}\n",
            ulid(99)
        );
        let err = parse_and_validate(&yaml, &no_existing()).unwrap_err();
        assert!(
            err.error.contains("item.complete") && err.error.contains("unknown item_id"),
            "expected item.complete error: {}",
            err.error
        );
    }

    #[test]
    fn item_expire_unknown_id_errors() {
        let yaml = format!(
            "events:\n  - event_type: item.expire\n    payload:\n      item_id: {}\n      reason: retracted\n",
            ulid(99)
        );
        let err = parse_and_validate(&yaml, &no_existing()).unwrap_err();
        assert!(
            err.error.contains("item.expire") && err.error.contains("unknown item_id"),
            "expected item.expire error: {}",
            err.error
        );
    }

    #[test]
    fn validation_error_display_returns_inner_message() {
        let err = ValidationError {
            error: "schema mismatch".into(),
            raw_output: "raw bytes".into(),
        };
        assert_eq!(err.to_string(), "schema mismatch");
    }

    #[test]
    fn item_complete_with_existing_id_is_accepted() {
        let existing_id = ulid(7);
        let mut existing = HashSet::new();
        existing.insert(existing_id);
        let yaml = format!(
            "events:\n  - event_type: item.complete\n    payload:\n      item_id: {existing_id}\n      note: shipped\n"
        );
        let events = parse_and_validate(&yaml, &existing).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn item_expire_with_reason_ttl_is_rejected() {
        let existing_id = ulid(7);
        let mut existing = HashSet::new();
        existing.insert(existing_id);
        let yaml = format!(
            "events:\n  - event_type: item.expire\n    payload:\n      item_id: {existing_id}\n      reason: ttl\n"
        );
        let err = parse_and_validate(&yaml, &existing).unwrap_err();
        assert!(
            err.error.contains("reason: ttl is reserved"),
            "got: {}",
            err.error
        );
    }

    #[test]
    fn item_expire_superseded_requires_superseded_by() {
        let existing_id = ulid(7);
        let mut existing = HashSet::new();
        existing.insert(existing_id);
        let yaml = format!(
            "events:\n  - event_type: item.expire\n    payload:\n      item_id: {existing_id}\n      reason: superseded\n"
        );
        let err = parse_and_validate(&yaml, &existing).unwrap_err();
        assert!(err.error.contains("superseded_by"), "got: {}", err.error);
    }

    #[test]
    fn item_expire_retracted_with_superseded_by_is_rejected() {
        let existing_id = ulid(7);
        let mut existing = HashSet::new();
        existing.insert(existing_id);
        let yaml = format!(
            "events:\n  - event_type: item.expire\n    payload:\n      item_id: {existing_id}\n      reason: retracted\n      superseded_by: {}\n",
            ulid(8)
        );
        let err = parse_and_validate(&yaml, &existing).unwrap_err();
        assert!(
            err.error.contains("superseded_by is only valid"),
            "got: {}",
            err.error
        );
    }

    #[test]
    fn duplicate_item_create_id_is_rejected() {
        let new_id = ulid(1);
        let yaml = format!(
            "events:\n  - event_type: item.create\n    payload:\n      item_id: {new_id}\n      class: todo\n      text: a\n  - event_type: item.create\n    payload:\n      item_id: {new_id}\n      class: todo\n      text: b\n"
        );
        let err = parse_and_validate(&yaml, &no_existing()).unwrap_err();
        assert!(
            err.error.contains("duplicate item_id"),
            "got: {}",
            err.error
        );
    }

    #[test]
    fn item_create_id_colliding_with_existing_state_is_rejected() {
        let id = ulid(1);
        let mut existing = HashSet::new();
        existing.insert(id);
        let yaml = format!(
            "events:\n  - event_type: item.create\n    payload:\n      item_id: {id}\n      class: todo\n      text: a\n"
        );
        let err = parse_and_validate(&yaml, &existing).unwrap_err();
        assert!(err.error.contains("collides"), "got: {}", err.error);
    }

    #[test]
    fn malformed_yaml_returns_validation_error_carrying_raw_output() {
        let yaml = "this is not: valid yaml\n  - unbalanced";
        let err = parse_and_validate(yaml, &no_existing()).unwrap_err();
        assert!(
            err.error.contains("YAML parse failure"),
            "got: {}",
            err.error
        );
        assert_eq!(err.raw_output, yaml);
    }

    #[test]
    fn unknown_event_type_errors_via_serde() {
        let yaml = "events:\n  - event_type: item.invent\n    payload:\n      item_id: nonsense\n";
        let err = parse_and_validate(yaml, &no_existing()).unwrap_err();
        assert!(
            err.error.contains("YAML parse failure"),
            "got: {}",
            err.error
        );
    }

    #[test]
    fn decision_record_does_not_need_existing_ids() {
        let yaml = format!(
            "events:\n  - event_type: decision.record\n    payload:\n      decision_id: {}\n      text: choose ULIDs\n",
            ulid(1)
        );
        let events = parse_and_validate(&yaml, &no_existing()).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn research_note_does_not_need_existing_ids() {
        let yaml = format!(
            "events:\n  - event_type: research.note\n    payload:\n      note_id: {}\n      text: assemblyai is immutable-finals\n",
            ulid(1)
        );
        let events = parse_and_validate(&yaml, &no_existing()).unwrap();
        assert_eq!(events.len(), 1);
    }
}
