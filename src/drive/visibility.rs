//! Drive move visibility-diff algorithm — the security-critical core of
//! `drive move`'s safety gate ([ADR-0070](../../docs/adrs/adr-0070.md)).
//!
//! Deliberately pure: zero `DriveClient`/network dependency, and zero
//! dependency on `crate::drive::file_move` (or any other engine module) —
//! this module classifies an already-fetched set of permission snapshots,
//! so the whole file is unit-testable with no wiremock at all.
//! `DrivePermission` fetching lives in `crate::drive::permissions_api`;
//! orchestration (which files to fetch, calling [`classify`], gating the
//! move) lives in `crate::drive::file_move`.
//!
//! # The algorithm
//!
//! Drive's `permissions.list(fileId)` returns a file's full *effective*
//! permission set (direct + inherited, merged) — but the
//! `permissionDetails[].inherited` flag that would let us split "direct"
//! from "inherited" is only populated for Shared Drive items, not My Drive
//! files. So instead of reading that split off the file directly, it's
//! derived by subtraction, from three snapshots the caller fetches:
//!
//! ```text
//! before          = principal_set(permissions.list(file_id))
//! current_parent  = principal_set(permissions.list(union of file's current parent(s)))  // ∅ if none
//! dest            = principal_set(permissions.list(dest_folder_id))
//!
//! direct_on_file  = before − current_parent      // what's granted on the file, not inherited
//! after           = direct_on_file ∪ dest
//!
//! added   = after − before     // visibility increase: new principals gain access
//! removed = before − after     // visibility decrease: principals lose access
//! ```
//!
//! Multi-parent legacy files: the caller unions permissions across *every*
//! current parent, not just the first, before calling [`diff_visibility`].
//! An orphan/root file with no current parent passes an empty
//! `current_parent_perms` slice, which degenerates correctly to "everything
//! on the file counts as direct."
//!
//! # Known limitation: shadowed grants
//!
//! If a principal has *both* a direct grant on the file and inherited
//! access via the current parent (a "shadowed" grant), the subtraction
//! can't distinguish them: `direct_on_file` won't include that principal
//! (it's in `current_parent` too), so if `dest` also doesn't grant it,
//! `after` won't include it either — [`diff_visibility`] reports it as
//! losing access even though it actually keeps it via the shadowed direct
//! grant, invisible to this subtraction.
//!
//! This is the **safe failure direction**: it can only produce a **false
//! positive** on `removed` (an unnecessary `--allow-visibility-decrease`
//! requirement), **never a false negative** on `added` — `direct_on_file ⊆
//! before` always holds, and `dest` enters `after` unfiltered, so a real
//! visibility increase is always caught. Accepted, not a bug to fix later.

use std::collections::BTreeSet;

use crate::drive::types::DrivePermission;

/// A Drive permission's identity, independent of `role`.
///
/// `role` is deliberately excluded from this key: a role change for an
/// already-visible principal (e.g. `reader` → `writer`) doesn't gate a move
/// in v1 — only set membership (added/removed) does — but is still visible
/// informationally via `DrivePermission::role` on the raw snapshots, for
/// logging that wants it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Principal {
    /// A specific Google account, keyed by email.
    User(String),
    /// A Google Group, keyed by email.
    Group(String),
    /// Every account in a Google Workspace domain.
    Domain(String),
    /// "Anyone with the link" — public.
    Anyone,
}

/// Builds the set of principals a permission list grants access to.
///
/// Unrecognised `DrivePermission::permission_type` values are skipped
/// rather than erroring — forward-compatible with a Drive permission type
/// this module doesn't yet model, at the cost of that permission never
/// contributing to a diff (fail-safe: an unmodelled *grant* is simply
/// invisible to `added`/`removed`, never miscounted as either).
#[must_use]
pub fn principal_set(perms: &[DrivePermission]) -> BTreeSet<Principal> {
    perms.iter().filter_map(principal_of).collect()
}

fn principal_of(perm: &DrivePermission) -> Option<Principal> {
    match perm.permission_type.as_str() {
        "user" => perm.email_address.clone().map(Principal::User),
        "group" => perm.email_address.clone().map(Principal::Group),
        "domain" => perm.domain.clone().map(Principal::Domain),
        "anyone" => Some(Principal::Anyone),
        _ => None,
    }
}

/// The result of diffing a file's visibility before/after a hypothetical
/// move — see the module doc for the algorithm and its known limitation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VisibilityDiff {
    /// Principals gaining access.
    pub added: BTreeSet<Principal>,
    /// Principals losing access (may include false positives from a
    /// shadowed direct+inherited grant — see the module doc).
    pub removed: BTreeSet<Principal>,
}

/// Computes the visibility diff a move from the file's current parent(s) to
/// a destination folder would produce, given three already-fetched
/// permission snapshots (see the module doc's algorithm).
#[must_use]
pub fn diff_visibility(
    file_perms: &[DrivePermission],
    current_parent_perms: &[DrivePermission],
    dest_folder_perms: &[DrivePermission],
) -> VisibilityDiff {
    let before = principal_set(file_perms);
    let current_parent = principal_set(current_parent_perms);
    let dest = principal_set(dest_folder_perms);

    let direct_on_file: BTreeSet<Principal> = before.difference(&current_parent).cloned().collect();
    let after: BTreeSet<Principal> = direct_on_file.union(&dest).cloned().collect();

    let added = after.difference(&before).cloned().collect();
    let removed = before.difference(&after).cloned().collect();

    VisibilityDiff { added, removed }
}

/// Which safety gate(s) block a move.
///
/// A struct of bools, not a single-variant enum: a move can simultaneously
/// fail more than one gate, and the audit log should say so precisely
/// rather than reporting only the first match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockReasons {
    /// `diff.added` was non-empty and `allow_visibility_increase` wasn't set.
    pub visibility_increase: bool,
    /// `diff.removed` was non-empty and `allow_visibility_decrease` wasn't
    /// set.
    pub visibility_decrease: bool,
    /// The move crosses a My Drive / Shared Drive boundary and
    /// `allow_drive_boundary_crossing` wasn't set.
    pub drive_boundary_crossing: bool,
}

impl BlockReasons {
    /// Whether any gate is blocking.
    #[must_use]
    pub fn any(self) -> bool {
        self.visibility_increase || self.visibility_decrease || self.drive_boundary_crossing
    }
}

/// The three independent, per-move opt-ins [`classify`] gates on.
///
/// A small struct local to this module (not `crate::drive::file_move`'s
/// eventual `MoveOptions`, which also carries the destination folder id and
/// other orchestration-only fields) — keeps this module's public API free
/// of more than clippy's bool-parameter limit and free of any dependency on
/// `file_move`, consistent with staying pure and independently testable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MoveGateFlags {
    /// Allows a move that would grant new principals access.
    pub allow_visibility_increase: bool,
    /// Allows a move that would revoke existing principals' access.
    pub allow_visibility_decrease: bool,
    /// Allows a move across a My Drive / Shared Drive boundary.
    pub allow_drive_boundary_crossing: bool,
}

/// Classifies whether a move is clear to proceed.
///
/// Returns `None` when clear; `Some(reasons)` naming every gate that blocks
/// it.
#[must_use]
pub fn classify(
    diff: &VisibilityDiff,
    crosses_boundary: bool,
    flags: MoveGateFlags,
) -> Option<BlockReasons> {
    let reasons = BlockReasons {
        visibility_increase: !diff.added.is_empty() && !flags.allow_visibility_increase,
        visibility_decrease: !diff.removed.is_empty() && !flags.allow_visibility_decrease,
        drive_boundary_crossing: crosses_boundary && !flags.allow_drive_boundary_crossing,
    };
    reasons.any().then_some(reasons)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn user(email: &str) -> DrivePermission {
        DrivePermission {
            id: format!("perm-{email}"),
            permission_type: "user".to_string(),
            role: "reader".to_string(),
            email_address: Some(email.to_string()),
            domain: None,
        }
    }

    fn group(email: &str) -> DrivePermission {
        DrivePermission {
            permission_type: "group".to_string(),
            email_address: Some(email.to_string()),
            ..user(email)
        }
    }

    fn domain(name: &str) -> DrivePermission {
        DrivePermission {
            id: format!("perm-{name}"),
            permission_type: "domain".to_string(),
            role: "reader".to_string(),
            email_address: None,
            domain: Some(name.to_string()),
        }
    }

    fn anyone() -> DrivePermission {
        DrivePermission {
            id: "perm-anyone".to_string(),
            permission_type: "anyone".to_string(),
            role: "reader".to_string(),
            email_address: None,
            domain: None,
        }
    }

    // ── principal_set ────────────────────────────────────────────────

    #[test]
    fn principal_set_maps_every_recognized_type() {
        let perms = vec![
            user("alice@example.com"),
            group("team@example.com"),
            domain("example.com"),
            anyone(),
        ];
        let set = principal_set(&perms);
        assert!(set.contains(&Principal::User("alice@example.com".to_string())));
        assert!(set.contains(&Principal::Group("team@example.com".to_string())));
        assert!(set.contains(&Principal::Domain("example.com".to_string())));
        assert!(set.contains(&Principal::Anyone));
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn principal_set_skips_unrecognized_types() {
        let mut weird = user("alice@example.com");
        weird.permission_type = "somethingNew".to_string();
        let set = principal_set(&[weird]);
        assert!(set.is_empty());
    }

    #[test]
    fn principal_set_skips_user_with_no_email_address() {
        let mut malformed = user("alice@example.com");
        malformed.email_address = None;
        let set = principal_set(&[malformed]);
        assert!(set.is_empty());
    }

    #[test]
    fn principal_set_is_empty_for_no_permissions() {
        assert!(principal_set(&[]).is_empty());
    }

    // ── diff_visibility ──────────────────────────────────────────────

    // `file_perms` (the `before` snapshot) is `permissions.list(file_id)`'s
    // full *merged* effective set — direct-on-file ∪ inherited-from-parent
    // — not "direct grants only". When a test file has no extra direct
    // grant beyond what it inherits, `file_perms` equals `current_parent`
    // exactly (that equality is what lets the subtraction find nothing
    // "direct" to preserve).

    #[test]
    fn diff_visibility_no_change_when_dest_grants_same_as_current_parent() {
        let current_parent = vec![user("alice@example.com")];
        let file = current_parent.clone(); // nothing direct beyond inheritance
        let dest = vec![user("alice@example.com")];
        let diff = diff_visibility(&file, &current_parent, &dest);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_visibility_detects_a_pure_increase() {
        let current_parent = vec![user("alice@example.com")];
        let file = current_parent.clone();
        let dest = vec![user("alice@example.com"), user("bob@example.com")];
        let diff = diff_visibility(&file, &current_parent, &dest);
        assert_eq!(
            diff.added,
            BTreeSet::from([Principal::User("bob@example.com".to_string())])
        );
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_visibility_detects_a_pure_decrease() {
        let current_parent = vec![user("alice@example.com"), user("bob@example.com")];
        let file = current_parent.clone();
        let dest = vec![user("alice@example.com")];
        let diff = diff_visibility(&file, &current_parent, &dest);
        assert!(diff.added.is_empty());
        assert_eq!(
            diff.removed,
            BTreeSet::from([Principal::User("bob@example.com".to_string())])
        );
    }

    #[test]
    fn diff_visibility_detects_both_an_increase_and_a_decrease() {
        let current_parent = vec![user("alice@example.com")];
        let file = current_parent.clone();
        let dest = vec![user("bob@example.com")];
        let diff = diff_visibility(&file, &current_parent, &dest);
        assert_eq!(
            diff.added,
            BTreeSet::from([Principal::User("bob@example.com".to_string())])
        );
        assert_eq!(
            diff.removed,
            BTreeSet::from([Principal::User("alice@example.com".to_string())])
        );
    }

    #[test]
    fn diff_visibility_preserves_a_direct_grant_on_the_file_across_the_move() {
        // alice has a direct grant on the file itself, not inherited from
        // the current parent — moving to a dest that grants nobody must
        // not report her as losing access.
        let file = vec![user("alice@example.com")];
        let current_parent = vec![];
        let dest = vec![];
        let diff = diff_visibility(&file, &current_parent, &dest);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_visibility_orphan_file_with_no_current_parent_treats_everything_as_direct() {
        let file = vec![user("alice@example.com")];
        let current_parent = vec![]; // no current parent at all
        let dest = vec![];
        let diff = diff_visibility(&file, &current_parent, &dest);
        // alice's grant is on the file itself (nothing to subtract), so it
        // survives the move regardless of what dest grants.
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_visibility_unions_permissions_across_multiple_current_parents() {
        // A multi-parent legacy file: alice is only visible via the SECOND
        // parent. Passing only the first parent's permissions would
        // wrongly treat alice's access as direct-on-file and report a
        // spurious `removed` when it's actually inherited and about to be
        // lost. The caller is responsible for unioning both parents'
        // permissions before calling this function — this test documents
        // that contract by doing the union inline.
        let parent_a_perms = vec![user("carol@example.com")];
        let parent_b_perms = vec![user("alice@example.com")];
        let current_parent: Vec<DrivePermission> =
            parent_a_perms.into_iter().chain(parent_b_perms).collect();
        let file = current_parent.clone(); // nothing direct beyond inheritance
        let dest = vec![user("carol@example.com")]; // drops alice
        let diff = diff_visibility(&file, &current_parent, &dest);
        assert_eq!(
            diff.removed,
            BTreeSet::from([Principal::User("alice@example.com".to_string())])
        );
    }

    #[test]
    fn diff_visibility_shadowed_grant_only_ever_produces_a_false_positive_on_removed() {
        // alice has BOTH a direct grant on the file AND inherited access
        // via the current parent (the shadowed case the module doc
        // describes). dest grants nobody. The subtraction can't see the
        // direct grant (it's masked by current_parent), so this
        // over-reports her as losing access — a false positive on
        // `removed` — but critically never produces a false negative on
        // `added` for anyone else in the same scenario.
        let file = vec![user("alice@example.com")]; // direct grant
        let current_parent = vec![user("alice@example.com")]; // same principal, inherited
        let dest = vec![user("bob@example.com")]; // a real, independent increase
        let diff = diff_visibility(&file, &current_parent, &dest);
        // False positive: alice is reported as removed even though her
        // direct grant would actually survive the move.
        assert!(diff
            .removed
            .contains(&Principal::User("alice@example.com".to_string())));
        // No false negative: the real increase (bob) is still caught.
        assert_eq!(
            diff.added,
            BTreeSet::from([Principal::User("bob@example.com".to_string())])
        );
    }

    #[test]
    fn diff_visibility_no_op_move_reports_no_change() {
        let perms = vec![user("alice@example.com"), domain("example.com")];
        let diff = diff_visibility(&perms, &perms, &perms);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    // ── classify ─────────────────────────────────────────────────────

    fn diff_with(added: &[&str], removed: &[&str]) -> VisibilityDiff {
        VisibilityDiff {
            added: added
                .iter()
                .map(|e| Principal::User((*e).to_string()))
                .collect(),
            removed: removed
                .iter()
                .map(|e| Principal::User((*e).to_string()))
                .collect(),
        }
    }

    fn flags(increase: bool, decrease: bool, boundary: bool) -> MoveGateFlags {
        MoveGateFlags {
            allow_visibility_increase: increase,
            allow_visibility_decrease: decrease,
            allow_drive_boundary_crossing: boundary,
        }
    }

    #[test]
    fn classify_is_clear_when_nothing_changes_and_no_boundary_crossing() {
        let diff = diff_with(&[], &[]);
        assert_eq!(classify(&diff, false, flags(false, false, false)), None);
    }

    #[test]
    fn classify_blocks_an_unallowed_increase() {
        let diff = diff_with(&["bob@example.com"], &[]);
        let reasons = classify(&diff, false, flags(false, false, false)).unwrap();
        assert!(reasons.visibility_increase);
        assert!(!reasons.visibility_decrease);
        assert!(!reasons.drive_boundary_crossing);
    }

    #[test]
    fn classify_allows_an_increase_when_opted_in() {
        let diff = diff_with(&["bob@example.com"], &[]);
        assert_eq!(classify(&diff, false, flags(true, false, false)), None);
    }

    #[test]
    fn classify_blocks_an_unallowed_decrease() {
        let diff = diff_with(&[], &["alice@example.com"]);
        let reasons = classify(&diff, false, flags(false, false, false)).unwrap();
        assert!(!reasons.visibility_increase);
        assert!(reasons.visibility_decrease);
        assert!(!reasons.drive_boundary_crossing);
    }

    #[test]
    fn classify_allows_a_decrease_when_opted_in() {
        let diff = diff_with(&[], &["alice@example.com"]);
        assert_eq!(classify(&diff, false, flags(false, true, false)), None);
    }

    #[test]
    fn classify_blocks_an_unallowed_boundary_crossing_even_with_no_visibility_change() {
        let diff = diff_with(&[], &[]);
        let reasons = classify(&diff, true, flags(false, false, false)).unwrap();
        assert!(!reasons.visibility_increase);
        assert!(!reasons.visibility_decrease);
        assert!(reasons.drive_boundary_crossing);
    }

    #[test]
    fn classify_allows_a_boundary_crossing_when_opted_in() {
        let diff = diff_with(&[], &[]);
        assert_eq!(classify(&diff, true, flags(false, false, true)), None);
    }

    #[test]
    fn classify_reports_every_simultaneously_failing_gate() {
        let diff = diff_with(&["bob@example.com"], &["alice@example.com"]);
        let reasons = classify(&diff, true, flags(false, false, false)).unwrap();
        assert!(reasons.visibility_increase);
        assert!(reasons.visibility_decrease);
        assert!(reasons.drive_boundary_crossing);
    }

    #[test]
    fn classify_allows_when_every_relevant_flag_is_opted_in() {
        let diff = diff_with(&["bob@example.com"], &["alice@example.com"]);
        assert_eq!(classify(&diff, true, flags(true, true, true)), None);
    }

    #[test]
    fn block_reasons_any_is_false_by_default() {
        assert!(!BlockReasons::default().any());
    }
}
