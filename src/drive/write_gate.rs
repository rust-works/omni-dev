//! Folder-scoped write-permission gate (issue #1574).
//!
//! `omni-dev`'s own local policy layer bounding `drive create`/`upload`/
//! `edit`, independent of and enforced *in addition to* whatever the OAuth
//! scope (`crate::drive::auth::DriveGrantedScopes`) would technically
//! allow. Google's Drive scopes are all-or-nothing across a user's whole
//! Drive — there is no Google-side way to say "this credential may only
//! write inside folder X" — so this module fills that gap.
//!
//! Deliberately pure: zero `DriveClient`/network dependency, mirroring
//! `crate::drive::visibility`'s contract exactly. Fetching the ancestor
//! folder chain a target lives in is `crate::drive::folder_ancestry`'s job;
//! this module only classifies an already-resolved chain.
//!
//! Named `write_gate`, not `permission(s)`, to avoid any confusion with
//! `crate::drive::permissions_api` — Google's own sharing/ACL wrapper,
//! a completely unrelated concept.
//!
//! # The algorithm
//!
//! A target (the `--parent` folder for `create`/`upload`, or a file's
//! current parent folder(s) for `edit`) is identified by its **ancestor
//! chain**: `chain[0]` is the target folder itself, `chain[1]` its parent,
//! `chain[2]` its grandparent, and so on up to Drive's root. [`resolve`]
//! walks that chain looking for the closest (lowest-depth) rule naming any
//! folder in it — a non-recursive rule only ever matches at depth 0, its
//! own folder; a recursive rule matches at any depth. Two tie-breaks, both
//! security-relevant and each covered by a dedicated test:
//!
//! - **Closest ancestor wins**: a rule on a subfolder overrides a broader
//!   rule on its parent — the more specific grant/restriction is assumed
//!   the more deliberate one.
//! - **Deny beats allow at equal depth**: if two rules at the same depth
//!   disagree, the safe direction wins.
//!
//! When no rule anywhere in the chain names `op`, [`DriveOperation::default_policy`]
//! decides it: `Read` defaults to [`Verdict::Allow`], every write operation
//! defaults to [`Verdict::Deny`]. This is where "disabled by default" for
//! writes actually lives — there is deliberately no separate enabled/
//! disabled toggle; an absent or empty rule list already means "deny
//! everywhere" via this table alone.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// A Drive operation this gate can permit or refuse. Reused directly as the
/// settings-file rule shape (`crate::utils::settings::WritePermissionsSettings`)
/// — no separate wire type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriveOperation {
    /// List/read/export/download.
    Read,
    /// Create a new file or folder.
    Create,
    /// Upload local content into a new file.
    Upload,
    /// Replace an existing file's content.
    Edit,
    /// Write cells into an existing Google Sheet via the Sheets API
    /// (issue #1589, [ADR-0073](../../docs/adrs/adr-0073.md) §3).
    ///
    /// Deliberately **not** folded into [`Self::Edit`]. Every existing
    /// `allow: ["edit"]` rule was written when `drive edit` refused every
    /// Google-native document outright, so reusing `Edit` here would
    /// retroactively upgrade those rules into cell-write permission with no
    /// config change and no re-consent — the exact silent widening this
    /// gate's default-deny posture exists to prevent.
    SheetsWrite,
}

impl std::fmt::Display for DriveOperation {
    /// Matches the `#[serde(rename_all = "kebab-case")]` wire form — used
    /// by `drive permissions show`/`check`'s rendering.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Read => "read",
            Self::Create => "create",
            Self::Upload => "upload",
            Self::Edit => "edit",
            Self::SheetsWrite => "sheets-write",
        };
        write!(f, "{s}")
    }
}

impl DriveOperation {
    /// The verdict when no configured rule names this operation anywhere in
    /// a target's ancestor chain. `Read` stays open by default (unchanged
    /// from today's behavior); every write defaults closed — the whole
    /// "disabled by default" requirement lives in this one match, not a
    /// separate on/off flag.
    fn default_policy(self) -> Verdict {
        match self {
            Self::Read => Verdict::Allow,
            Self::Create | Self::Upload | Self::Edit | Self::SheetsWrite => Verdict::Deny,
        }
    }
}

/// One configured folder rule.
///
/// `folder_id` is Drive's own canonical id, not a path — Drive folders
/// have no stable, unique path (names collide, files can have multiple
/// legacy parents), so identity is the id, exactly as the browser
/// bridge's `OriginAllowlist` matches exact origin strings rather than
/// URL patterns.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FolderPermissionRule {
    /// The Drive folder id this rule matches.
    pub folder_id: String,
    /// When `true`, this rule also matches every descendant of `folder_id`,
    /// not just the folder itself (depth > 0 in the ancestor chain). A
    /// non-recursive rule only ever matches at depth 0.
    #[serde(default)]
    pub recursive: bool,
    /// Operations explicitly permitted at this folder.
    #[serde(default)]
    pub allow: HashSet<DriveOperation>,
    /// Operations explicitly refused at this folder.
    #[serde(default)]
    pub deny: HashSet<DriveOperation>,
}

/// The result of resolving a single operation against a rule set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The operation is permitted.
    Allow,
    /// The operation is refused.
    Deny,
}

/// Which configured rule (if any) decided a [`Decision`].
///
/// `None` means no rule matched anywhere in the chain and the bare
/// default policy decided it instead. Carried into the request log's
/// `decided_by_folder_id`/`decided_by_depth` context fields so a refusal
/// is exactly as auditable as a success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecidingRule {
    /// The folder id of the rule that decided the verdict.
    pub folder_id: String,
    /// How many levels above the target this rule's folder sits (0 = the
    /// target itself).
    pub depth: usize,
}

/// The outcome of [`resolve`]: whether an operation is permitted, and which
/// rule (if any) decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Whether the operation is permitted.
    pub verdict: Verdict,
    /// The configured rule that decided this, or `None` when the bare
    /// default policy decided it instead.
    pub decided_by: Option<DecidingRule>,
}

/// Resolves whether `op` is permitted against `chain` (depth 0 = the
/// target folder itself, then parent, grandparent, ...) under `rules`. See
/// the module doc for the full algorithm and its tie-breaks.
#[must_use]
pub fn resolve(chain: &[String], op: DriveOperation, rules: &[FolderPermissionRule]) -> Decision {
    let mut best: Option<(usize, Verdict)> = None;
    for (depth, folder_id) in chain.iter().enumerate() {
        let mut deny_here = false;
        let mut allow_here = false;
        for rule in rules {
            if rule.folder_id != *folder_id {
                continue;
            }
            if depth > 0 && !rule.recursive {
                continue;
            }
            deny_here |= rule.deny.contains(&op);
            allow_here |= rule.allow.contains(&op);
        }
        if !deny_here && !allow_here {
            continue;
        }
        let verdict = if deny_here {
            Verdict::Deny
        } else {
            Verdict::Allow
        };
        let is_closer = match best {
            Some((best_depth, _)) => depth < best_depth,
            None => true,
        };
        if is_closer {
            best = Some((depth, verdict));
        }
    }
    match best {
        Some((depth, verdict)) => Decision {
            verdict,
            decided_by: Some(DecidingRule {
                folder_id: chain[depth].clone(),
                depth,
            }),
        },
        None => Decision {
            verdict: op.default_policy(),
            decided_by: None,
        },
    }
}

/// Combines per-parent [`Decision`]s into one, for a target with more than
/// one current parent (a legacy multi-parent file — Drive no longer
/// permits creating new ones).
///
/// Deny wins across parents, the same fail-closed direction every other
/// tie-break in this module takes. Callers with a single parent (the
/// common case) can call [`resolve`] directly instead; an *orphan* target
/// (zero parents) should call `resolve(&[], op, rules)` directly too,
/// rather than calling this with zero decisions — a target's chain
/// degenerates to "only the default policy applies," not to a policy
/// about *no* chain, so `first` requires at least one decision by
/// construction.
///
/// Shared by `drive edit` (whose target's chain starts at its *current*
/// parents, unioned) and `drive permissions check` (whose target may
/// itself be a file).
#[must_use]
pub fn combine_across_parents(
    first: Decision,
    rest: impl IntoIterator<Item = Decision>,
) -> Decision {
    rest.into_iter().fold(first, |acc, next| {
        if acc.verdict == Verdict::Deny {
            acc
        } else {
            next
        }
    })
}

/// Splits an optional [`DecidingRule`] into the `(folder_id, depth)` pair
/// `crate::request_log::DriveMutationOutcome::decided_by_folder_id`/
/// `decided_by_depth` expect.
///
/// Shared by `create`/`upload`/`edit`'s otherwise near-identical
/// `record_attempt` functions — each still builds its own
/// `DriveMutationOutcome` (the verb-specific fields genuinely differ), but
/// this was the one piece of extraction logic that was byte-for-byte the
/// same in all three. Kept here (rather than in `crate::request_log`,
/// which stays decoupled from any one integration's internal types) and
/// pure, matching this module's zero-I/O contract — it does not itself
/// call `crate::request_log::record_drive_mutation`.
#[must_use]
pub fn decided_by_log_fields(decided_by: Option<&DecidingRule>) -> (Option<String>, Option<usize>) {
    match decided_by {
        Some(rule) => (Some(rule.folder_id.clone()), Some(rule.depth)),
        None => (None, None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn rule(
        folder_id: &str,
        recursive: bool,
        allow: &[DriveOperation],
        deny: &[DriveOperation],
    ) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: folder_id.to_string(),
            recursive,
            allow: allow.iter().copied().collect(),
            deny: deny.iter().copied().collect(),
        }
    }

    fn chain(ids: &[&str]) -> Vec<String> {
        ids.iter().copied().map(ToString::to_string).collect()
    }

    #[test]
    fn default_policy_allows_read_with_no_rules() {
        let decision = resolve(&chain(&["a"]), DriveOperation::Read, &[]);
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(decision.decided_by, None);
    }

    #[test]
    fn default_policy_denies_create_upload_edit_with_no_rules() {
        for op in [
            DriveOperation::Create,
            DriveOperation::Upload,
            DriveOperation::Edit,
        ] {
            let decision = resolve(&chain(&["a"]), op, &[]);
            assert_eq!(
                decision.verdict,
                Verdict::Deny,
                "{op:?} should default-deny"
            );
            assert_eq!(decision.decided_by, None);
        }
    }

    #[test]
    fn recursive_rule_matches_deep_descendant() {
        let rules = [rule("root", true, &[DriveOperation::Create], &[])];
        let decision = resolve(
            &chain(&["child", "grandchild", "root"]),
            DriveOperation::Create,
            &rules,
        );
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(decision.decided_by.unwrap().folder_id, "root");
    }

    #[test]
    fn non_recursive_rule_matches_own_folder_only() {
        let rules = [rule("target", false, &[DriveOperation::Create], &[])];
        let decision = resolve(&chain(&["target"]), DriveOperation::Create, &rules);
        assert_eq!(decision.verdict, Verdict::Allow);
    }

    #[test]
    fn non_recursive_rule_does_not_match_child() {
        let rules = [rule("parent", false, &[DriveOperation::Create], &[])];
        let decision = resolve(&chain(&["child", "parent"]), DriveOperation::Create, &rules);
        // Falls through to the default policy since the non-recursive rule
        // never matches at depth > 0.
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.decided_by, None);
    }

    #[test]
    fn closest_ancestor_wins_deny_over_broader_allow() {
        let rules = [
            rule("child", true, &[], &[DriveOperation::Create]),
            rule("parent", true, &[DriveOperation::Create], &[]),
        ];
        let decision = resolve(&chain(&["child", "parent"]), DriveOperation::Create, &rules);
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.decided_by.unwrap().folder_id, "child");
    }

    #[test]
    fn closest_ancestor_wins_allow_over_broader_deny() {
        // The inverse case — proves "closest wins" isn't secretly "deny
        // always wins": a closer *allow* beats a farther *deny*.
        let rules = [
            rule("child", true, &[DriveOperation::Create], &[]),
            rule("parent", true, &[], &[DriveOperation::Create]),
        ];
        let decision = resolve(&chain(&["child", "parent"]), DriveOperation::Create, &rules);
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(decision.decided_by.unwrap().folder_id, "child");
    }

    #[test]
    fn deny_beats_allow_at_equal_depth() {
        let rules = [
            rule("target", false, &[DriveOperation::Create], &[]),
            rule("target", false, &[], &[DriveOperation::Create]),
        ];
        let decision = resolve(&chain(&["target"]), DriveOperation::Create, &rules);
        assert_eq!(decision.verdict, Verdict::Deny);
    }

    #[test]
    fn rule_on_unrelated_folder_does_not_apply() {
        let rules = [rule("unrelated", true, &[DriveOperation::Create], &[])];
        let decision = resolve(
            &chain(&["target", "parent"]),
            DriveOperation::Create,
            &rules,
        );
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.decided_by, None);
    }

    #[test]
    fn empty_chain_orphan_file_uses_default_policy_only() {
        let rules = [rule("some-folder", true, &[DriveOperation::Create], &[])];
        let decision = resolve(&[], DriveOperation::Create, &rules);
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.decided_by, None);
    }

    #[test]
    fn display_matches_the_serde_kebab_case_wire_form() {
        // Display is hand-written, so it can drift from the derive. Every
        // variant is asserted against a round-trip through serde rather
        // than a second hand-written literal, which would drift with it.
        for op in [
            DriveOperation::Read,
            DriveOperation::Create,
            DriveOperation::Upload,
            DriveOperation::Edit,
            DriveOperation::SheetsWrite,
        ] {
            let wire = serde_json::to_string(&op).unwrap();
            assert_eq!(
                format!("\"{op}\""),
                wire,
                "Display drifted from serde for {op:?}"
            );
        }
        // The pre-existing spellings are pinned literally: switching
        // `rename_all` to kebab-case must not have moved any of them.
        assert_eq!(DriveOperation::Read.to_string(), "read");
        assert_eq!(DriveOperation::Create.to_string(), "create");
        assert_eq!(DriveOperation::Upload.to_string(), "upload");
        assert_eq!(DriveOperation::Edit.to_string(), "edit");
        assert_eq!(DriveOperation::SheetsWrite.to_string(), "sheets-write");
    }

    #[test]
    fn sheets_write_defaults_to_deny_like_every_other_write() {
        let decision = resolve(&chain(&["f"]), DriveOperation::SheetsWrite, &[]);
        assert_eq!(decision.verdict, Verdict::Deny);
        assert!(decision.decided_by.is_none());
    }

    #[test]
    fn an_edit_rule_does_not_grant_sheets_write() {
        // The whole reason for a separate variant: an existing
        // `allow: ["edit"]` rule must not silently gain cell-write power.
        let rules = [rule("target", true, &[DriveOperation::Edit], &[])];
        let edit = resolve(&chain(&["target"]), DriveOperation::Edit, &rules);
        let sheets = resolve(&chain(&["target"]), DriveOperation::SheetsWrite, &rules);
        assert_eq!(edit.verdict, Verdict::Allow);
        assert_eq!(sheets.verdict, Verdict::Deny);
    }

    #[test]
    fn operations_on_one_rule_are_independent() {
        let rules = [rule("target", false, &[DriveOperation::Create], &[])];
        let create = resolve(&chain(&["target"]), DriveOperation::Create, &rules);
        let upload = resolve(&chain(&["target"]), DriveOperation::Upload, &rules);
        assert_eq!(create.verdict, Verdict::Allow);
        assert_eq!(
            upload.verdict,
            Verdict::Deny,
            "an allow:[create] rule must not leak into upload"
        );
    }

    #[test]
    fn deny_list_and_allow_list_on_same_rule_apply_to_different_ops_independently() {
        let rules = [rule(
            "target",
            false,
            &[DriveOperation::Create],
            &[DriveOperation::Edit],
        )];
        let create = resolve(&chain(&["target"]), DriveOperation::Create, &rules);
        let edit = resolve(&chain(&["target"]), DriveOperation::Edit, &rules);
        let upload = resolve(&chain(&["target"]), DriveOperation::Upload, &rules);
        assert_eq!(create.verdict, Verdict::Allow);
        assert_eq!(edit.verdict, Verdict::Deny);
        assert_eq!(
            upload.verdict,
            Verdict::Deny,
            "no rule named upload; falls to default policy"
        );
    }

    // ── combine_across_parents ────────────────────────────────────────

    fn decision(verdict: Verdict) -> Decision {
        Decision {
            verdict,
            decided_by: None,
        }
    }

    #[test]
    fn combine_across_parents_single_decision_returns_it_unchanged() {
        let combined = combine_across_parents(decision(Verdict::Allow), []);
        assert_eq!(combined.verdict, Verdict::Allow);
    }

    #[test]
    fn combine_across_parents_deny_beats_allow_deny_first() {
        let combined = combine_across_parents(decision(Verdict::Deny), [decision(Verdict::Allow)]);
        assert_eq!(combined.verdict, Verdict::Deny);
    }

    #[test]
    fn combine_across_parents_deny_beats_allow_allow_first() {
        let combined = combine_across_parents(decision(Verdict::Allow), [decision(Verdict::Deny)]);
        assert_eq!(combined.verdict, Verdict::Deny);
    }

    // ── decided_by_log_fields ────────────────────────────────────────

    #[test]
    fn decided_by_log_fields_none_yields_none_pair() {
        assert_eq!(decided_by_log_fields(None), (None, None));
    }

    #[test]
    fn decided_by_log_fields_some_extracts_folder_id_and_depth() {
        let rule = DecidingRule {
            folder_id: "folder-1".to_string(),
            depth: 2,
        };
        assert_eq!(
            decided_by_log_fields(Some(&rule)),
            (Some("folder-1".to_string()), Some(2))
        );
    }

    #[test]
    fn combine_across_parents_all_allow_returns_allow() {
        let combined = combine_across_parents(
            decision(Verdict::Allow),
            [decision(Verdict::Allow), decision(Verdict::Allow)],
        );
        assert_eq!(combined.verdict, Verdict::Allow);
    }
}
