//! Reflection event schema (the `events.jsonl` contract from #799).
//!
//! These types form the wire format for the append-only reflection log
//! produced by `voice reflect` (this issue) and consumed by `voice review`
//! (#804). The shape is the load-bearing contract — see the umbrella
//! issue #799 for the full design rationale and reconciliation
//! invariants. The [`project`] helper here implements the subset of those
//! invariants that `voice reflect` itself needs (to render the
//! `<current_state>` block in its prompt). TTL eviction lives in `voice
//! review` and is not implemented here.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::warn;

use crate::voice::EventId;

/// Identifier for an item (todo / research / question).
pub type ItemId = ulid::Ulid;

/// Identifier for a recorded decision.
pub type DecisionId = ulid::Ulid;

/// Identifier for a standalone research note.
pub type NoteId = ulid::Ulid;

/// Identifies the reflection invocation that produced a batch of events.
///
/// `Ulid(_)` for events emitted by `voice reflect`; `Review` for events
/// written by reconciliation (`voice review`, #804) — currently just
/// `item.expire { reason: ttl }`. Serialised on the wire as either a
/// ULID string or the literal `"review"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReflectionId {
    /// A specific reflection invocation.
    Ulid(ulid::Ulid),
    /// Emitted by reconciliation, not by a reflection.
    Review,
}

impl Serialize for ReflectionId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Ulid(u) => s.serialize_str(&u.to_string()),
            Self::Review => s.serialize_str("review"),
        }
    }
}

impl<'de> Deserialize<'de> for ReflectionId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "review" {
            Ok(Self::Review)
        } else {
            ulid::Ulid::from_string(&s)
                .map(Self::Ulid)
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Range of consumed `TranscriptEvent::Final` IDs that motivated an event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptSpan {
    /// First `Final` event in the consumed transcript range.
    pub start_event_id: EventId,
    /// Last `Final` event in the consumed transcript range.
    pub end_event_id: EventId,
}

/// What motivated an event — for audit and reconciliation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    /// Range of transcript events this reflection consumed (null for
    /// review-emitted events, which consume no transcript).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub transcript_span: Option<TranscriptSpan>,
    /// Model identifier (null for review-emitted events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Prompt-template fingerprint (null for review-emitted events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<String>,
}

/// Class of an item — present-tense intent the user expressed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemClass {
    /// A thing the user wants done.
    Todo,
    /// Background context worth keeping but not actionable.
    Research,
    /// An open question the user wants answered.
    Question,
}

/// Priority of an item.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Bumped above the rest of the list.
    High,
    /// The default.
    Normal,
    /// Demoted below the rest of the list.
    Low,
}

/// Why an item expired.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExpireReason {
    /// User explicitly retracted the item.
    Retracted,
    /// `valid_until` elapsed (emitted by `voice review`, not `reflect`).
    Ttl,
    /// Replaced by a more recent item — see `superseded_by`.
    Superseded,
}

/// `item.create` payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemCreate {
    /// Stable identifier minted by the LLM, referenced by later events.
    pub item_id: ItemId,
    /// Class of the item.
    pub class: ItemClass,
    /// Item text.
    pub text: String,
    /// Optional priority; absent means [`Priority::Normal`] at projection.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub priority: Option<Priority>,
    /// Optional expiry time; absent means "use class default" at projection.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub valid_until: Option<DateTime<Utc>>,
    /// Optional tags.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tags: Option<Vec<String>>,
}

/// `item.update` payload. All fields besides `item_id` are optional — any
/// present field denotes a change.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ItemUpdate {
    /// Identifier of the existing item to update.
    pub item_id: ItemId,
    /// New text (optional).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
    /// New priority (optional).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub priority: Option<Priority>,
    /// New expiry (optional; sliding extension on re-mention).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub valid_until: Option<DateTime<Utc>>,
    /// Replacement tags (optional).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tags: Option<Vec<String>>,
}

/// `item.expire` payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemExpire {
    /// Identifier of the item to expire.
    pub item_id: ItemId,
    /// Why the item is being expired.
    pub reason: ExpireReason,
    /// New item replacing this one — present iff `reason == Superseded`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub superseded_by: Option<ItemId>,
}

/// `item.complete` payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemComplete {
    /// Identifier of the completed item.
    pub item_id: ItemId,
    /// Optional completion note.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}

/// `decision.record` payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionRecord {
    /// Stable identifier for this decision.
    pub decision_id: DecisionId,
    /// Decision text.
    pub text: String,
    /// Optional list of alternatives that were considered.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub alternatives: Option<Vec<String>>,
}

/// `research.note` payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchNote {
    /// Stable identifier for this note.
    pub note_id: NoteId,
    /// Note text.
    pub text: String,
    /// Optional supporting links.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub links: Option<Vec<String>>,
    /// Optional expiry (default P30D at projection).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub valid_until: Option<DateTime<Utc>>,
}

/// `reflection.error` payload — captured for audit when the LLM output
/// fails schema validation. Skipped by projection.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReflectionError {
    /// Raw LLM output that failed validation.
    pub raw_output: String,
    /// Human-readable error description.
    pub error: String,
}

/// Tagged union of event payloads, paired with the `event_type`
/// discriminator in the on-wire envelope.
///
/// The serde representation is *adjacently tagged* so the envelope ends
/// up with sibling `event_type` and `payload` fields — matching #799's
/// example shape. Flattening this enum into [`Event`] then puts those
/// fields at the same level as the envelope fields (`event_id`, `ts`, …).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event_type", content = "payload")]
pub enum EventKind {
    /// Mint a new todo / research note / question.
    #[serde(rename = "item.create")]
    ItemCreate(ItemCreate),
    /// Refine an existing item.
    #[serde(rename = "item.update")]
    ItemUpdate(ItemUpdate),
    /// Item no longer applies.
    #[serde(rename = "item.expire")]
    ItemExpire(ItemExpire),
    /// User completed the item.
    #[serde(rename = "item.complete")]
    ItemComplete(ItemComplete),
    /// A decision was made.
    #[serde(rename = "decision.record")]
    DecisionRecord(DecisionRecord),
    /// Standalone research note.
    #[serde(rename = "research.note")]
    ResearchNote(ResearchNote),
    /// Schema-invalid LLM output captured for audit.
    #[serde(rename = "reflection.error")]
    ReflectionError(ReflectionError),
}

/// One event in the `events.jsonl` log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    /// Sortable, monotonic, no collisions. Used as the canonical
    /// ordering key by reconciliation.
    pub event_id: EventId,
    /// Emission time (UTC RFC3339).
    pub ts: DateTime<Utc>,
    /// Identifies the reflection invocation that produced this event.
    pub reflection_id: ReflectionId,
    /// What motivated the event.
    pub provenance: Provenance,
    /// Discriminated payload + type tag (flattened into the envelope so
    /// `event_type` and `payload` are top-level keys, per #799).
    #[serde(flatten)]
    pub kind: EventKind,
}

// ─── Projection ────────────────────────────────────────────────────────

/// Materialised item, as projected from the event log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedItem {
    /// Identifier of the item.
    pub id: ItemId,
    /// Class (todo / research / question).
    pub class: ItemClass,
    /// Current text.
    pub text: String,
    /// Current priority (defaults to [`Priority::Normal`] when unspecified).
    pub priority: Priority,
    /// Current expiry, if explicitly set.
    pub valid_until: Option<DateTime<Utc>>,
    /// Current tags.
    pub tags: Vec<String>,
    /// `true` when an `item.complete` has been seen.
    pub completed: bool,
    /// `Some(reason)` when an `item.expire` has been seen.
    pub expired: Option<ExpireReason>,
}

/// Materialised decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedDecision {
    /// Identifier of the decision.
    pub id: DecisionId,
    /// Decision text.
    pub text: String,
    /// Alternatives that were considered.
    pub alternatives: Vec<String>,
}

/// Materialised research note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedNote {
    /// Identifier of the note.
    pub id: NoteId,
    /// Note text.
    pub text: String,
    /// Supporting links.
    pub links: Vec<String>,
    /// Expiry, if set.
    pub valid_until: Option<DateTime<Utc>>,
}

/// Materialised state after applying an event log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectedState {
    /// Items keyed by id. Includes completed and expired items so callers
    /// can filter as they see fit (`voice reflect` excludes them from
    /// `<current_state>`; `voice review` may render them under a separate
    /// heading).
    pub items: BTreeMap<ItemId, ProjectedItem>,
    /// Decisions in insertion order.
    pub decisions: Vec<ProjectedDecision>,
    /// Notes in insertion order.
    pub notes: Vec<ProjectedNote>,
}

/// Projects a sequence of [`Event`]s into a [`ProjectedState`] per #799's
/// reconciliation invariants:
///
/// - Sort by `event_id` (ULIDs are sortable).
/// - Last-write-wins per item field on `item.update`.
/// - Idempotent expiry.
/// - Unknown `item_id` in `item.update` / `item.expire` / `item.complete`
///   → drop the operation, log a warning.
/// - `reflection.error` skipped entirely.
///
/// **Not** implemented here (deferred to `voice review` / #804):
/// - TTL eviction (synthesising `item.expire { reason: ttl }`).
pub fn project<I: IntoIterator<Item = Event>>(events: I) -> ProjectedState {
    let mut sorted: Vec<Event> = events.into_iter().collect();
    sorted.sort_by_key(|e| e.event_id);
    let mut state = ProjectedState::default();
    for event in sorted {
        apply_event(&mut state, event);
    }
    state
}

fn apply_event(state: &mut ProjectedState, event: Event) {
    match event.kind {
        EventKind::ItemCreate(c) => {
            state.items.insert(
                c.item_id,
                ProjectedItem {
                    id: c.item_id,
                    class: c.class,
                    text: c.text,
                    priority: c.priority.unwrap_or(Priority::Normal),
                    valid_until: c.valid_until,
                    tags: c.tags.unwrap_or_default(),
                    completed: false,
                    expired: None,
                },
            );
        }
        EventKind::ItemUpdate(u) => {
            let Some(item) = state.items.get_mut(&u.item_id) else {
                warn!(
                    item_id = %u.item_id,
                    "item.update references unknown item; dropping per #799 invariant"
                );
                return;
            };
            if let Some(text) = u.text {
                item.text = text;
            }
            if let Some(priority) = u.priority {
                item.priority = priority;
            }
            if u.valid_until.is_some() {
                item.valid_until = u.valid_until;
            }
            if let Some(tags) = u.tags {
                item.tags = tags;
            }
        }
        EventKind::ItemExpire(e) => {
            let Some(item) = state.items.get_mut(&e.item_id) else {
                warn!(
                    item_id = %e.item_id,
                    "item.expire references unknown item; dropping per #799 invariant"
                );
                return;
            };
            // Idempotent: ignore if already expired.
            if item.expired.is_none() {
                item.expired = Some(e.reason);
            }
        }
        EventKind::ItemComplete(c) => {
            let Some(item) = state.items.get_mut(&c.item_id) else {
                warn!(
                    item_id = %c.item_id,
                    "item.complete references unknown item; dropping per #799 invariant"
                );
                return;
            };
            item.completed = true;
        }
        EventKind::DecisionRecord(d) => {
            state.decisions.push(ProjectedDecision {
                id: d.decision_id,
                text: d.text,
                alternatives: d.alternatives.unwrap_or_default(),
            });
        }
        EventKind::ResearchNote(n) => {
            state.notes.push(ProjectedNote {
                id: n.note_id,
                text: n.text,
                links: n.links.unwrap_or_default(),
                valid_until: n.valid_until,
            });
        }
        EventKind::ReflectionError(_) => {
            // Skipped by projection — captured in the log for audit only.
        }
    }
}

/// Default TTLs per item class (per #799). Used when an `item.create`
/// omits `valid_until`. Callers compute the effective expiry as
/// `event.ts + class_default_ttl(item.class)`.
#[must_use]
pub fn class_default_ttl(class: &ItemClass) -> Duration {
    match class {
        ItemClass::Todo => Duration::from_secs(7 * 24 * 60 * 60),
        ItemClass::Research => Duration::from_secs(30 * 24 * 60 * 60),
        ItemClass::Question => Duration::from_secs(14 * 24 * 60 * 60),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn span() -> TranscriptSpan {
        TranscriptSpan {
            start_event_id: ulid::Ulid::from_parts(0, 1),
            end_event_id: ulid::Ulid::from_parts(0, 2),
        }
    }

    fn provenance() -> Provenance {
        Provenance {
            transcript_span: Some(span()),
            model: Some("claude-sonnet-4-6".to_string()),
            prompt_version: Some("abcd1234".to_string()),
        }
    }

    fn event(event_id: u128, kind: EventKind) -> Event {
        Event {
            event_id: ulid::Ulid::from_parts(0, event_id),
            ts: ts(),
            reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 100)),
            provenance: provenance(),
            kind,
        }
    }

    fn id(n: u128) -> ItemId {
        ulid::Ulid::from_parts(0, n)
    }

    #[test]
    fn item_create_serialises_with_adjacent_tag() {
        let e = event(
            10,
            EventKind::ItemCreate(ItemCreate {
                item_id: id(200),
                class: ItemClass::Todo,
                text: "wire it up".to_string(),
                priority: None,
                valid_until: None,
                tags: None,
            }),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(
            json.contains(r#""event_type":"item.create""#),
            "missing event_type discriminator: {json}"
        );
        assert!(
            json.contains(r#""payload":{"#),
            "missing payload object: {json}"
        );
        assert!(
            json.contains(r#""class":"todo""#),
            "missing class enum rename: {json}"
        );
    }

    #[test]
    fn event_round_trips_through_serde_json() {
        let original = event(
            10,
            EventKind::ItemCreate(ItemCreate {
                item_id: id(200),
                class: ItemClass::Research,
                text: "look into LocalAgreement-2".to_string(),
                priority: Some(Priority::High),
                valid_until: Some(ts()),
                tags: Some(vec!["asr".to_string(), "whisper".to_string()]),
            }),
        );
        let s = serde_json::to_string(&original).unwrap();
        let back: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn reflection_id_review_serialises_as_string_literal() {
        let id = ReflectionId::Review;
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, r#""review""#);
        let back: ReflectionId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ReflectionId::Review);
    }

    #[test]
    fn reflection_id_ulid_round_trips() {
        let u = ulid::Ulid::from_parts(0, 42);
        let id = ReflectionId::Ulid(u);
        let s = serde_json::to_string(&id).unwrap();
        let back: ReflectionId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ReflectionId::Ulid(u));
    }

    #[test]
    fn project_orders_by_event_id_not_insertion_order() {
        // Insert items out of order; projection should sort them.
        let item_a = id(1);
        let events = vec![
            event(
                20,
                EventKind::ItemUpdate(ItemUpdate {
                    item_id: item_a,
                    text: Some("third write".to_string()),
                    ..Default::default()
                }),
            ),
            event(
                10,
                EventKind::ItemCreate(ItemCreate {
                    item_id: item_a,
                    class: ItemClass::Todo,
                    text: "first write".to_string(),
                    priority: None,
                    valid_until: None,
                    tags: None,
                }),
            ),
            event(
                15,
                EventKind::ItemUpdate(ItemUpdate {
                    item_id: item_a,
                    text: Some("second write".to_string()),
                    ..Default::default()
                }),
            ),
        ];
        let state = project(events);
        assert_eq!(state.items.get(&item_a).unwrap().text, "third write");
    }

    #[test]
    fn project_drops_update_for_unknown_item() {
        let state = project(vec![event(
            10,
            EventKind::ItemUpdate(ItemUpdate {
                item_id: id(999),
                text: Some("no such item".to_string()),
                ..Default::default()
            }),
        )]);
        assert!(state.items.is_empty());
    }

    #[test]
    fn project_applies_all_item_update_fields() {
        let i = id(1);
        let state = project(vec![
            event(
                10,
                EventKind::ItemCreate(ItemCreate {
                    item_id: i,
                    class: ItemClass::Todo,
                    text: "original".into(),
                    priority: None,
                    valid_until: None,
                    tags: None,
                }),
            ),
            event(
                11,
                EventKind::ItemUpdate(ItemUpdate {
                    item_id: i,
                    text: Some("updated".into()),
                    priority: Some(Priority::High),
                    valid_until: Some(ts()),
                    tags: Some(vec!["urgent".into()]),
                }),
            ),
        ]);
        let item = state.items.get(&i).unwrap();
        assert_eq!(item.text, "updated");
        assert_eq!(item.priority, Priority::High);
        assert_eq!(item.valid_until, Some(ts()));
        assert_eq!(item.tags, vec!["urgent".to_string()]);
    }

    #[test]
    fn project_drops_expire_and_complete_for_unknown_items() {
        let state = project(vec![
            event(
                10,
                EventKind::ItemExpire(ItemExpire {
                    item_id: id(99),
                    reason: ExpireReason::Retracted,
                    superseded_by: None,
                }),
            ),
            event(
                11,
                EventKind::ItemComplete(ItemComplete {
                    item_id: id(99),
                    note: None,
                }),
            ),
        ]);
        assert!(state.items.is_empty());
    }

    #[test]
    fn project_marks_completed_and_expired() {
        let i = id(1);
        let state = project(vec![
            event(
                10,
                EventKind::ItemCreate(ItemCreate {
                    item_id: i,
                    class: ItemClass::Todo,
                    text: "x".into(),
                    priority: None,
                    valid_until: None,
                    tags: None,
                }),
            ),
            event(
                11,
                EventKind::ItemComplete(ItemComplete {
                    item_id: i,
                    note: Some("done".into()),
                }),
            ),
            event(
                12,
                EventKind::ItemExpire(ItemExpire {
                    item_id: i,
                    reason: ExpireReason::Superseded,
                    superseded_by: Some(id(2)),
                }),
            ),
        ]);
        let item = state.items.get(&i).unwrap();
        assert!(item.completed);
        assert_eq!(item.expired, Some(ExpireReason::Superseded));
    }

    #[test]
    fn project_expire_is_idempotent() {
        let i = id(1);
        let state = project(vec![
            event(
                10,
                EventKind::ItemCreate(ItemCreate {
                    item_id: i,
                    class: ItemClass::Todo,
                    text: "x".into(),
                    priority: None,
                    valid_until: None,
                    tags: None,
                }),
            ),
            event(
                11,
                EventKind::ItemExpire(ItemExpire {
                    item_id: i,
                    reason: ExpireReason::Retracted,
                    superseded_by: None,
                }),
            ),
            event(
                12,
                EventKind::ItemExpire(ItemExpire {
                    item_id: i,
                    reason: ExpireReason::Superseded,
                    superseded_by: Some(id(2)),
                }),
            ),
        ]);
        // First expire wins; second is a no-op.
        assert_eq!(
            state.items.get(&i).unwrap().expired,
            Some(ExpireReason::Retracted)
        );
    }

    #[test]
    fn project_skips_reflection_errors() {
        let state = project(vec![event(
            10,
            EventKind::ReflectionError(ReflectionError {
                raw_output: "garbage".into(),
                error: "missing item_id".into(),
            }),
        )]);
        assert!(state.items.is_empty());
        assert!(state.decisions.is_empty());
        assert!(state.notes.is_empty());
    }

    #[test]
    fn project_appends_decisions_and_notes() {
        let state = project(vec![
            event(
                10,
                EventKind::DecisionRecord(DecisionRecord {
                    decision_id: id(1),
                    text: "use ULIDs".into(),
                    alternatives: Some(vec!["UUIDv7".into()]),
                }),
            ),
            event(
                11,
                EventKind::ResearchNote(ResearchNote {
                    note_id: id(2),
                    text: "AssemblyAI is immutable-finals".into(),
                    links: None,
                    valid_until: None,
                }),
            ),
        ]);
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.notes.len(), 1);
    }

    #[test]
    fn class_default_ttls_match_799() {
        assert_eq!(
            class_default_ttl(&ItemClass::Todo),
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(
            class_default_ttl(&ItemClass::Research),
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(
            class_default_ttl(&ItemClass::Question),
            Duration::from_secs(14 * 86_400)
        );
    }
}
