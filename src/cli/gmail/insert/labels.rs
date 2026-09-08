//! System-label replay and `--label` resolution for `gmail insert`.
//!
//! Replaying an archived message's labels verbatim is what satisfies issue
//! #1655's "preserve sent mail" requirement for free: `messages.batchModify`
//! **forbids** adding `SENT` (or `DRAFT`), which is exactly why `insert` is
//! the only Gmail write operation able to restore mail the mailbox sent
//! rather than received. But not every label a source mailbox carried is
//! safe or meaningful to replay onto a destination:
//!
//! - **User labels never survive.** A source mailbox's user-created label
//!   ids (`Label_1`, etc.) are foreign to the destination — they don't
//!   exist there, or worse, collide with an unrelated label of the same id.
//!   Only Gmail's fixed *system* labels replay; see [`SYSTEM_LABEL_ALLOWLIST`].
//! - **`DRAFT` is excluded** despite being a system label: inserting a
//!   message with it creates a row in Drafts with no backing Draft
//!   resource, which the destination account can't properly edit or send.
//! - **`TRASH`/`SPAM` replay, but the engine counts them into a `Note`**
//!   before the fan-out (not this module's job) — trashed mail auto-purges
//!   after 30 days, so a "restore" that silently lands 400 messages in
//!   Trash quietly destroys them a month later.
//! - **`INBOX` is the biggest UX landmine**, not special-cased here: rather
//!   than guessing at a default, `--drop-label` lets a caller strip it (and
//!   `UNREAD` alongside it) after the system filter, and the engine emits a
//!   `Note` with the affected count so `--dry-run` shows it up front.

use anyhow::Result;

use crate::gmail::types::Label;

/// Gmail system labels safe to replay onto a destination mailbox.
/// `CATEGORY_*` (Promotions/Social/Updates/Forums/Personal) isn't listed
/// literally — see [`is_replayable_system_label`] — since it's an open
/// family, not a fixed enumeration.
const SYSTEM_LABEL_ALLOWLIST: &[&str] = &[
    "INBOX",
    "SENT",
    "UNREAD",
    "STARRED",
    "IMPORTANT",
    "SPAM",
    "TRASH",
];

/// Whether `label_id` is one of Gmail's replayable system labels — an
/// allow-list, not an `is_uppercase` heuristic (a user label id can also be
/// all-uppercase). `DRAFT` is a real Gmail system label but is deliberately
/// **not** in [`SYSTEM_LABEL_ALLOWLIST`] — see the module doc.
fn is_replayable_system_label(label_id: &str) -> bool {
    SYSTEM_LABEL_ALLOWLIST.contains(&label_id) || label_id.starts_with("CATEGORY_")
}

/// Computes the label id set to send with `messages.insert`: the archived
/// record's replayable system labels, minus `drop_label_ids`, plus
/// `destination_label_id` (the resolved `--label` tag) if given.
///
/// `drop_label_ids` is applied **after** the system-label filter, so
/// `--drop-label` only ever removes a label that would otherwise have been
/// replayed — naming a user label or an already-excluded `DRAFT` is a
/// silent no-op, not an error, since the end state (that label absent) is
/// identical either way.
pub(crate) fn resolve_label_ids(
    source_label_ids: &[String],
    drop_label_ids: &[String],
    destination_label_id: Option<&str>,
) -> Vec<String> {
    let mut ids: Vec<String> = source_label_ids
        .iter()
        .filter(|id| is_replayable_system_label(id))
        .filter(|id| !drop_label_ids.iter().any(|dropped| dropped == *id))
        .cloned()
        .collect();
    if let Some(label) = destination_label_id {
        if !ids.iter().any(|id| id == label) {
            ids.push(label.to_string());
        }
    }
    ids
}

/// Resolves `--label NAME` to a label id via an already-fetched
/// `labels.list` response. Fails **before any write**: `gmail insert` never
/// auto-creates a label, unlike a naive "add if missing" convenience —
/// silently creating the caller's tag on a typo would insert the whole
/// batch under a name nobody asked for.
pub(crate) fn resolve_label_id_by_name(labels: &[Label], name: &str) -> Result<String> {
    labels
        .iter()
        .find(|label| label.name == name)
        .map(|label| label.id.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "label {name:?} does not exist in the destination mailbox; `gmail insert` \
                 never auto-creates labels — create it first (e.g. `gmail label list`, or via \
                 Gmail's own label settings)"
            )
        })
}

/// Whether any of `label_ids` would land a message in a *live*, visible
/// mailbox view — the engine's pre-flight `Note` uses this to warn before
/// the fan-out, since faithful replay can otherwise dump thousands of
/// messages into an Inbox with no warning.
pub(crate) fn lands_in_inbox_or_unread(label_ids: &[String]) -> bool {
    label_ids.iter().any(|id| id == "INBOX" || id == "UNREAD")
}

/// Whether any of `label_ids` would land a message somewhere Gmail
/// auto-purges from — see the module doc's `TRASH`/`SPAM` note.
pub(crate) fn lands_in_trash_or_spam(label_ids: &[String]) -> bool {
    label_ids.iter().any(|id| id == "TRASH" || id == "SPAM")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn label(id: &str, name: &str) -> Label {
        Label {
            id: id.to_string(),
            name: name.to_string(),
            ..Label::default()
        }
    }

    // ── resolve_label_ids ────────────────────────────────────────────

    #[test]
    fn resolve_label_ids_keeps_only_allow_listed_system_labels() {
        let ids = resolve_label_ids(
            &[
                "INBOX".to_string(),
                "Label_1".to_string(),
                "IMPORTANT".to_string(),
            ],
            &[],
            None,
        );
        assert_eq!(ids, vec!["INBOX".to_string(), "IMPORTANT".to_string()]);
    }

    #[test]
    fn resolve_label_ids_excludes_draft_despite_being_a_system_label() {
        let ids = resolve_label_ids(&["DRAFT".to_string(), "INBOX".to_string()], &[], None);
        assert_eq!(ids, vec!["INBOX".to_string()]);
    }

    #[test]
    fn resolve_label_ids_replays_category_labels() {
        let ids = resolve_label_ids(&["CATEGORY_PROMOTIONS".to_string()], &[], None);
        assert_eq!(ids, vec!["CATEGORY_PROMOTIONS".to_string()]);
    }

    #[test]
    fn resolve_label_ids_applies_drop_label_after_the_system_filter() {
        let ids = resolve_label_ids(
            &["INBOX".to_string(), "UNREAD".to_string()],
            &["INBOX".to_string(), "UNREAD".to_string()],
            None,
        );
        assert!(ids.is_empty());
    }

    #[test]
    fn resolve_label_ids_dropping_a_user_label_is_a_silent_no_op() {
        let ids = resolve_label_ids(&["INBOX".to_string()], &["Label_1".to_string()], None);
        assert_eq!(ids, vec!["INBOX".to_string()]);
    }

    #[test]
    fn resolve_label_ids_unions_the_destination_label() {
        let ids = resolve_label_ids(&["INBOX".to_string()], &[], Some("Label_restore"));
        assert_eq!(ids, vec!["INBOX".to_string(), "Label_restore".to_string()]);
    }

    #[test]
    fn resolve_label_ids_does_not_duplicate_the_destination_label_if_already_present() {
        let ids = resolve_label_ids(&["Label_restore".to_string()], &[], Some("Label_restore"));
        // Not a system label, so the source copy is dropped by the filter —
        // then re-added exactly once by the union step.
        assert_eq!(ids, vec!["Label_restore".to_string()]);
    }

    // ── resolve_label_id_by_name ──────────────────────────────────────

    #[test]
    fn resolve_label_id_by_name_finds_a_match() {
        let labels = vec![label("Label_1", "Restored"), label("INBOX", "INBOX")];
        assert_eq!(
            resolve_label_id_by_name(&labels, "Restored").unwrap(),
            "Label_1"
        );
    }

    #[test]
    fn resolve_label_id_by_name_errors_without_auto_creating() {
        let labels = vec![label("INBOX", "INBOX")];
        let err = resolve_label_id_by_name(&labels, "Missing").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
        assert!(err.to_string().contains("never auto-creates"));
    }

    // ── lands_in_inbox_or_unread / lands_in_trash_or_spam ─────────────

    #[test]
    fn lands_in_inbox_or_unread_detects_either() {
        assert!(lands_in_inbox_or_unread(&["INBOX".to_string()]));
        assert!(lands_in_inbox_or_unread(&["UNREAD".to_string()]));
        assert!(!lands_in_inbox_or_unread(&["SENT".to_string()]));
    }

    #[test]
    fn lands_in_trash_or_spam_detects_either() {
        assert!(lands_in_trash_or_spam(&["TRASH".to_string()]));
        assert!(lands_in_trash_or_spam(&["SPAM".to_string()]));
        assert!(!lands_in_trash_or_spam(&["SENT".to_string()]));
    }
}
