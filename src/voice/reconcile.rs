//! Pure reconciliation of `events.jsonl` into materialised markdown.
//!
//! The `reconcile()` function is a pure function from event log to
//! markdown + new TTL-expiry events; the `voice review` CLI wrapper
//! (in [`crate::voice::review`]) handles all I/O around it.
//!
//! Reuses [`crate::voice::events::project`] for the bulk of the
//! reconciliation invariants from #799 (sort-by-event-id,
//! last-write-wins, idempotent expiry, unknown-id drop,
//! `reflection.error` skip) and adds TTL eviction on top: any
//! non-expired non-completed item whose effective expiry has elapsed
//! gets a synthetic `item.expire { reason: ttl, reflection_id: "review" }`
//! event minted into `new_expiry_events`. Synthetic events use the
//! injected [`UlidRng`] for ids so snapshot tests can pin them.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::voice::det::UlidRng;
use crate::voice::events::{
    project, DecisionId, Event, EventKind, ExpireReason, ItemClass, ItemExpire, ItemId,
    ProjectedDecision, ProjectedItem, Provenance, ReflectionId,
};
use crate::voice::render::{render_decisions_md, render_todos_md};
use crate::voice::session::TtlDefaults;
use crate::voice::EventId;

/// Markdown + new events produced by a single reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutput {
    /// Contents to write to `todos.md`.
    pub todos_md: String,
    /// Contents to write to `decisions.md`.
    pub decisions_md: String,
    /// Synthetic `item.expire { reason: ttl }` events to append to
    /// `events.jsonl`. Empty when no items have aged out.
    pub new_expiry_events: Vec<Event>,
}

/// Item plus the metadata reconciliation needs but projection drops:
/// the create-event id (for sort order) and the create-event timestamp
/// (for class-default TTL computation).
#[derive(Debug, Clone)]
pub struct ReconciledItem {
    /// The projected item (text, class, priority, valid_until, …).
    pub item: ProjectedItem,
    /// Event id of the `item.create` that minted the item — used as
    /// the within-bucket sort key in `todos.md`.
    pub created_event_id: EventId,
}

/// Decision plus its create-event id.
#[derive(Debug, Clone)]
pub struct ReconciledDecision {
    /// The projected decision (text, alternatives).
    pub decision: ProjectedDecision,
    /// Event id of the `decision.record` — used for the newest-first
    /// sort in `decisions.md`.
    pub created_event_id: EventId,
}

/// Reconciles an event log into materialised markdown and synthesised
/// TTL-expiry events.
///
/// Pure: no I/O, no clock reads. `now` and `rng` are injected so
/// snapshot tests can pin them.
///
/// `defaults` supplies class-default TTLs (used when an `item.create`
/// omits `valid_until` and no `item.update` ever set it).
pub fn reconcile(
    events: &[Event],
    defaults: &TtlDefaults,
    now: DateTime<Utc>,
    rng: &mut dyn UlidRng,
) -> ReviewOutput {
    // Projection handles every #799 invariant except TTL.
    let mut state = project(events.iter().cloned());

    // Walk events once to capture per-item / per-decision creation
    // metadata that projection doesn't surface (created_ts is needed
    // for class-default TTL; created_event_id for sort order).
    let mut item_created: HashMap<ItemId, (DateTime<Utc>, EventId)> = HashMap::new();
    let mut decision_created: HashMap<DecisionId, EventId> = HashMap::new();
    let mut sorted = events.to_vec();
    sorted.sort_by_key(|e| e.event_id);
    for event in &sorted {
        match &event.kind {
            EventKind::ItemCreate(c) => {
                item_created
                    .entry(c.item_id)
                    .or_insert((event.ts, event.event_id));
            }
            EventKind::DecisionRecord(d) => {
                decision_created
                    .entry(d.decision_id)
                    .or_insert(event.event_id);
            }
            _ => {}
        }
    }

    // TTL pass — synthesise `item.expire` for each non-expired,
    // non-completed item whose effective expiry has elapsed.
    let mut new_expiry_events = Vec::new();
    for (id, item) in &mut state.items {
        if item.expired.is_some() || item.completed {
            continue;
        }
        let Some((created_ts, _)) = item_created.get(id) else {
            // Item exists in projection without a create event — only
            // possible if projection's drop-unknown invariant lets a
            // bare update through, but it doesn't. Defensive skip.
            continue;
        };
        let effective_expiry = item
            .valid_until
            .unwrap_or_else(|| *created_ts + ttl_for(&item.class, defaults));
        if effective_expiry > now {
            continue;
        }
        let event = Event {
            event_id: rng.next_ulid(),
            ts: now,
            reflection_id: ReflectionId::Review,
            provenance: Provenance {
                transcript_span: None,
                model: None,
                prompt_version: None,
            },
            kind: EventKind::ItemExpire(ItemExpire {
                item_id: *id,
                reason: ExpireReason::Ttl,
                superseded_by: None,
            }),
        };
        new_expiry_events.push(event);
        item.expired = Some(ExpireReason::Ttl);
    }

    // Render todos: non-expired, non-completed, class ∈ {Todo, Question}.
    let todos: Vec<ReconciledItem> = state
        .items
        .values()
        .filter(|i| i.expired.is_none() && !i.completed)
        .filter(|i| matches!(i.class, ItemClass::Todo | ItemClass::Question))
        .filter_map(|i| {
            item_created.get(&i.id).map(|(_, eid)| ReconciledItem {
                item: i.clone(),
                created_event_id: *eid,
            })
        })
        .collect();
    let todos_md = render_todos_md(&todos);

    // Render decisions in insertion order (which is event-id order, by
    // construction of project()); render_decisions_md applies the
    // newest-first sort.
    let decisions: Vec<ReconciledDecision> = state
        .decisions
        .iter()
        .filter_map(|d| {
            decision_created.get(&d.id).map(|eid| ReconciledDecision {
                decision: d.clone(),
                created_event_id: *eid,
            })
        })
        .collect();
    let decisions_md = render_decisions_md(&decisions);

    ReviewOutput {
        todos_md,
        decisions_md,
        new_expiry_events,
    }
}

fn ttl_for(class: &ItemClass, defaults: &TtlDefaults) -> chrono::Duration {
    let std_dur = match class {
        ItemClass::Todo => defaults.todo,
        ItemClass::Research => defaults.research,
        ItemClass::Question => defaults.question,
    };
    // `chrono::Duration::from_std` only fails on durations exceeding
    // ~292 billion years; a malformed config that hits that bound
    // collapses to zero TTL (the corrupt-input-expires-immediately
    // policy).
    chrono::Duration::from_std(std_dur).unwrap_or(chrono::Duration::zero())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::det::CountingUlidRng;
    use crate::voice::events::{
        DecisionRecord, ItemComplete, ItemCreate, ItemUpdate, Priority, TranscriptSpan,
    };
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
    }

    fn provenance() -> Provenance {
        Provenance {
            transcript_span: Some(TranscriptSpan {
                start_event_id: ulid::Ulid::from_parts(0, 1),
                end_event_id: ulid::Ulid::from_parts(0, 2),
            }),
            model: Some("m".into()),
            prompt_version: Some("p".into()),
        }
    }

    fn event_at(event_id: u128, ts: DateTime<Utc>, kind: EventKind) -> Event {
        Event {
            event_id: ulid::Ulid::from_parts(0, event_id),
            ts,
            reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 100)),
            provenance: provenance(),
            kind,
        }
    }

    fn id(n: u128) -> ItemId {
        ulid::Ulid::from_parts(0, n)
    }

    fn create_todo(eid: u128, item: u128, text: &str, ts: DateTime<Utc>) -> Event {
        event_at(
            eid,
            ts,
            EventKind::ItemCreate(ItemCreate {
                item_id: id(item),
                class: ItemClass::Todo,
                text: text.into(),
                priority: None,
                valid_until: None,
                tags: None,
            }),
        )
    }

    #[test]
    fn ttl_expiry_emits_synthetic_event_for_overdue_item() {
        let created_ts = now() - chrono::Duration::days(10);
        let valid_until = now() - chrono::Duration::days(1);
        let events = vec![event_at(
            1,
            created_ts,
            EventKind::ItemCreate(ItemCreate {
                item_id: id(50),
                class: ItemClass::Todo,
                text: "expired".into(),
                priority: None,
                valid_until: Some(valid_until),
                tags: None,
            }),
        )];
        let mut rng = CountingUlidRng::new();
        let out = reconcile(&events, &TtlDefaults::default(), now(), &mut rng);
        assert_eq!(out.new_expiry_events.len(), 1);
        let e = &out.new_expiry_events[0];
        assert_eq!(e.reflection_id, ReflectionId::Review);
        assert!(e.provenance.transcript_span.is_none());
        assert!(e.provenance.model.is_none());
        assert!(e.provenance.prompt_version.is_none());
        match &e.kind {
            EventKind::ItemExpire(ie) => {
                assert_eq!(ie.item_id, id(50));
                assert_eq!(ie.reason, ExpireReason::Ttl);
            }
            other => panic!("expected ItemExpire, got {other:?}"),
        }
    }

    #[test]
    fn ttl_class_default_used_when_valid_until_absent() {
        // Todo default is 7 days; create at T-8d expires, T-5d does not.
        let stale = create_todo(1, 1, "stale", now() - chrono::Duration::days(8));
        let fresh = create_todo(2, 2, "fresh", now() - chrono::Duration::days(5));
        let mut rng = CountingUlidRng::new();
        let out = reconcile(&[stale, fresh], &TtlDefaults::default(), now(), &mut rng);
        assert_eq!(out.new_expiry_events.len(), 1);
        let expired_id = match &out.new_expiry_events[0].kind {
            EventKind::ItemExpire(ie) => ie.item_id,
            _ => panic!(),
        };
        assert_eq!(expired_id, id(1));
    }

    #[test]
    fn ttl_pass_idempotent() {
        let stale = create_todo(1, 1, "stale", now() - chrono::Duration::days(8));
        let mut rng = CountingUlidRng::new();
        let first = reconcile(
            std::slice::from_ref(&stale),
            &TtlDefaults::default(),
            now(),
            &mut rng,
        );
        assert_eq!(first.new_expiry_events.len(), 1);
        // Replay: the original event + the synthesised expiry event,
        // run through reconcile again — no new expiries should fire.
        let mut combined = vec![stale];
        combined.extend(first.new_expiry_events);
        let second = reconcile(&combined, &TtlDefaults::default(), now(), &mut rng);
        assert!(second.new_expiry_events.is_empty());
    }

    #[test]
    fn omits_completed_and_already_expired_items() {
        let e_create = create_todo(1, 1, "do it", now() - chrono::Duration::days(1));
        let e_complete = event_at(
            2,
            now(),
            EventKind::ItemComplete(ItemComplete {
                item_id: id(1),
                note: None,
            }),
        );
        let mut rng = CountingUlidRng::new();
        let out = reconcile(
            &[e_create, e_complete],
            &TtlDefaults::default(),
            now(),
            &mut rng,
        );
        assert!(out.new_expiry_events.is_empty());
        assert!(out.todos_md.lines().all(|l| !l.contains("do it")));
    }

    #[test]
    fn todos_md_groups_by_priority_and_sorts() {
        let high = event_at(
            1,
            now(),
            EventKind::ItemCreate(ItemCreate {
                item_id: id(10),
                class: ItemClass::Todo,
                text: "high one".into(),
                priority: Some(Priority::High),
                valid_until: None,
                tags: None,
            }),
        );
        let normal = create_todo(2, 11, "normal one", now());
        let low = event_at(
            3,
            now(),
            EventKind::ItemCreate(ItemCreate {
                item_id: id(12),
                class: ItemClass::Todo,
                text: "low one".into(),
                priority: Some(Priority::Low),
                valid_until: None,
                tags: None,
            }),
        );
        let mut rng = CountingUlidRng::new();
        let out = reconcile(
            &[normal, low, high],
            &TtlDefaults::default(),
            now(),
            &mut rng,
        );
        let high_pos = out.todos_md.find("high one").unwrap();
        let normal_pos = out.todos_md.find("normal one").unwrap();
        let low_pos = out.todos_md.find("low one").unwrap();
        assert!(high_pos < normal_pos && normal_pos < low_pos);
    }

    #[test]
    fn decisions_md_sorts_newest_first() {
        let older = event_at(
            5,
            now(),
            EventKind::DecisionRecord(DecisionRecord {
                decision_id: id(50),
                text: "older".into(),
                alternatives: None,
            }),
        );
        let newer = event_at(
            7,
            now(),
            EventKind::DecisionRecord(DecisionRecord {
                decision_id: id(51),
                text: "newer".into(),
                alternatives: Some(vec!["alt".into()]),
            }),
        );
        let mut rng = CountingUlidRng::new();
        let out = reconcile(&[older, newer], &TtlDefaults::default(), now(), &mut rng);
        let newer_pos = out.decisions_md.find("newer").unwrap();
        let older_pos = out.decisions_md.find("older").unwrap();
        assert!(newer_pos < older_pos);
    }

    #[test]
    fn update_keeps_valid_until_authoritative_for_ttl() {
        // Create at T-10d with explicit valid_until at T-1d (expired);
        // an update at T-5d bumps valid_until to T+5d. Reconciliation
        // should pick up the update and *not* synthesise an expiry.
        let create = event_at(
            1,
            now() - chrono::Duration::days(10),
            EventKind::ItemCreate(ItemCreate {
                item_id: id(1),
                class: ItemClass::Todo,
                text: "x".into(),
                priority: None,
                valid_until: Some(now() - chrono::Duration::days(1)),
                tags: None,
            }),
        );
        let update = event_at(
            2,
            now() - chrono::Duration::days(5),
            EventKind::ItemUpdate(ItemUpdate {
                item_id: id(1),
                valid_until: Some(now() + chrono::Duration::days(5)),
                ..Default::default()
            }),
        );
        let mut rng = CountingUlidRng::new();
        let out = reconcile(&[create, update], &TtlDefaults::default(), now(), &mut rng);
        assert!(
            out.new_expiry_events.is_empty(),
            "{:?}",
            out.new_expiry_events
        );
    }
}
