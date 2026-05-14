//! Library-level snapshot tests for `voice review`.
//!
//! Exercises [`omni_dev::voice::reconcile::reconcile`] directly with
//! curated event-log fixtures and golden-snapshot checks on the
//! rendered markdown — matching the issue's acceptance criterion that
//! `reconcile()` is the unit-of-tests for projection plus TTL.
//!
//! `CountingUlidRng` + `FixedClock` pin the synthetic-expiry event ids
//! and timestamps so snapshots are byte-stable across runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{DateTime, TimeZone, Utc};
use omni_dev::voice::det::CountingUlidRng;
use omni_dev::voice::events::{
    DecisionRecord, Event, EventKind, ItemClass, ItemCreate, ItemUpdate, Priority, Provenance,
    ReflectionId,
};
use omni_dev::voice::reconcile::reconcile;
use omni_dev::voice::session::TtlDefaults;

fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
}

fn event_at(eid: u128, ts: DateTime<Utc>, kind: EventKind) -> Event {
    Event {
        event_id: ulid::Ulid::from_parts(0, eid),
        ts,
        reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 100)),
        provenance: Provenance {
            transcript_span: None,
            model: Some("m".into()),
            prompt_version: Some("p".into()),
        },
        kind,
    }
}

fn id(n: u128) -> ulid::Ulid {
    ulid::Ulid::from_parts(0, n)
}

fn create(
    eid: u128,
    item: u128,
    class: ItemClass,
    text: &str,
    priority: Option<Priority>,
    valid_until: Option<DateTime<Utc>>,
    ts: DateTime<Utc>,
) -> Event {
    event_at(
        eid,
        ts,
        EventKind::ItemCreate(ItemCreate {
            item_id: id(item),
            class,
            text: text.into(),
            priority,
            valid_until,
            tags: None,
        }),
    )
}

#[test]
fn todos_md_groups_priorities_and_renders_question_prefix() {
    let now = fixed_now();
    let events = vec![
        // Out-of-order insertion to verify sort-by-create-event-id.
        create(3, 12, ItemClass::Todo, "normal todo", None, None, now),
        create(
            1,
            10,
            ItemClass::Todo,
            "high todo",
            Some(Priority::High),
            Some(now + chrono::Duration::days(2)),
            now,
        ),
        create(
            2,
            11,
            ItemClass::Question,
            "open question",
            Some(Priority::High),
            None,
            now,
        ),
        create(
            4,
            13,
            ItemClass::Todo,
            "low todo",
            Some(Priority::Low),
            None,
            now,
        ),
        // Research notes are out of scope for todos.md.
        create(
            5,
            14,
            ItemClass::Research,
            "background reading",
            None,
            None,
            now,
        ),
    ];

    let mut rng = CountingUlidRng::new();
    let out = reconcile(&events, &TtlDefaults::default(), now, &mut rng);

    insta::assert_snapshot!("voice_review_todos_priorities", out.todos_md);
}

#[test]
fn decisions_md_sorts_newest_first_and_omits_empty_alternatives() {
    let now = fixed_now();
    let events = vec![
        event_at(
            10,
            now,
            EventKind::DecisionRecord(DecisionRecord {
                decision_id: id(50),
                text: "use ULIDs".into(),
                alternatives: Some(vec!["UUIDv7".into(), "snowflake".into()]),
            }),
        ),
        event_at(
            20,
            now,
            EventKind::DecisionRecord(DecisionRecord {
                decision_id: id(51),
                text: "store events as JSONL".into(),
                alternatives: None,
            }),
        ),
        event_at(
            30,
            now,
            EventKind::DecisionRecord(DecisionRecord {
                decision_id: id(52),
                text: "review is a pure projection".into(),
                alternatives: Some(vec![]),
            }),
        ),
    ];

    let mut rng = CountingUlidRng::new();
    let out = reconcile(&events, &TtlDefaults::default(), now, &mut rng);

    insta::assert_snapshot!("voice_review_decisions_newest_first", out.decisions_md);
}

#[test]
fn ttl_pass_synthesises_expiry_events_for_aged_items() {
    let now = fixed_now();
    let events = vec![
        // Stale: explicit valid_until in the past → expires.
        create(
            1,
            1,
            ItemClass::Todo,
            "ship the PR",
            None,
            Some(now - chrono::Duration::hours(1)),
            now - chrono::Duration::days(3),
        ),
        // Stale via class default: todo created 9 days ago, default 7d.
        create(
            2,
            2,
            ItemClass::Todo,
            "follow up with reviewer",
            None,
            None,
            now - chrono::Duration::days(9),
        ),
        // Fresh: created today, no valid_until.
        create(3, 3, ItemClass::Todo, "answer feedback", None, None, now),
    ];

    let mut rng = CountingUlidRng::new();
    let out = reconcile(&events, &TtlDefaults::default(), now, &mut rng);

    assert_eq!(
        out.new_expiry_events.len(),
        2,
        "expected two synthesised expiry events, got {:?}",
        out.new_expiry_events
    );
    insta::assert_snapshot!("voice_review_todos_after_ttl", out.todos_md);
}

#[test]
fn update_extending_valid_until_keeps_item_alive() {
    let now = fixed_now();
    let create_event = create(
        1,
        1,
        ItemClass::Todo,
        "long-running",
        None,
        Some(now - chrono::Duration::days(1)),
        now - chrono::Duration::days(2),
    );
    let update_event = event_at(
        2,
        now - chrono::Duration::hours(1),
        EventKind::ItemUpdate(ItemUpdate {
            item_id: id(1),
            valid_until: Some(now + chrono::Duration::days(3)),
            ..Default::default()
        }),
    );
    let mut rng = CountingUlidRng::new();
    let out = reconcile(
        &[create_event, update_event],
        &TtlDefaults::default(),
        now,
        &mut rng,
    );
    assert!(
        out.new_expiry_events.is_empty(),
        "extended item should not be expired: {:?}",
        out.new_expiry_events
    );
    assert!(out.todos_md.contains("long-running"));
}
