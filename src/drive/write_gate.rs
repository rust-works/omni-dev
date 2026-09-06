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
//! current parent folder(s) for `edit`/`sheets write`) is identified by
//! its **ancestor chain**: `chain[0]` is the target folder itself,
//! `chain[1]` its parent, `chain[2]` its grandparent, and so on up to
//! Drive's root. [`resolve`] walks that chain looking for the closest
//! (lowest-depth) rule naming any folder in it — a non-recursive rule only
//! ever matches at depth 0, its own folder; a recursive rule matches at
//! any depth. Two tie-breaks, both security-relevant and each covered by a
//! dedicated test:
//!
//! - **Closest ancestor wins**: a rule on a subfolder overrides a broader
//!   rule on its parent — the more specific grant/restriction is assumed
//!   the more deliberate one.
//! - **Deny beats allow at equal depth**: if two rules at the same depth
//!   disagree, the safe direction wins.
//!
//! A rule may instead name a **file id** (issue #1612), which matches the
//! target itself at **depth −1** — strictly closer than depth 0, so it
//! beats every folder rule in both directions: a file `deny` overrides a
//! recursive folder `allow`, and a file `allow` overrides a folder `deny`.
//! That is the same "closest wins" principle, not an exception to it. Two
//! consequences worth stating outright:
//!
//! - Because nothing in any chain can beat a file rule, a decisive one
//!   short-circuits the ancestor walk entirely — see
//!   [`resolve_for_file`] and
//!   `crate::drive::folder_ancestry::resolve_decision_for_file_target`.
//!   This is what makes a file **shared by link or email** grantable at
//!   all: `files.get` returns only the parents the caller can see, so such
//!   a file has none, and no folder rule could ever apply to it.
//! - `combine_across_parents`' "deny wins" is a tie-break among *peers* —
//!   a target's several legacy parents, all at the same level. A file rule
//!   is not a peer, so a file `allow` still beats a denying parent.
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

/// One configured permission rule, keyed on **either** a folder id or a
/// file id — exactly one of the two, enforced when the settings file is
/// deserialized (see [`RawPermissionRule`]).
///
/// Ids are Drive's own canonical ids, not paths — Drive objects have no
/// stable, unique path (names collide, files can have multiple legacy
/// parents), so identity is the id, exactly as the browser bridge's
/// `OriginAllowlist` matches exact origin strings rather than URL
/// patterns.
///
/// A `file_id` rule is what makes a file shared by link or email
/// grantable at all (issue #1612): `files.get` returns only the parents
/// this account can *see*, so such a file arrives with none, and no
/// folder rule could ever apply to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawPermissionRule")]
pub struct FolderPermissionRule {
    /// The Drive folder id this rule matches, for a folder rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    /// The Drive file id this rule matches, for a file rule. Matched at
    /// depth −1 against the target itself, before any ancestor walk — see
    /// [`resolve_file_rule`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    /// When `true`, this rule also matches every descendant of
    /// `folder_id`, not just the folder itself (depth > 0 in the ancestor
    /// chain). A non-recursive rule only ever matches at depth 0.
    /// Meaningless — and rejected — on a `file_id` rule: a file has no
    /// descendants.
    pub recursive: bool,
    /// Operations explicitly permitted at this target.
    pub allow: HashSet<DriveOperation>,
    /// Operations explicitly refused at this target.
    pub deny: HashSet<DriveOperation>,
}

impl FolderPermissionRule {
    /// A folder rule naming `folder_id`.
    #[must_use]
    pub fn folder(folder_id: impl Into<String>) -> Self {
        Self {
            folder_id: Some(folder_id.into()),
            file_id: None,
            recursive: false,
            allow: HashSet::new(),
            deny: HashSet::new(),
        }
    }

    /// A file rule naming `file_id`.
    #[must_use]
    pub fn file(file_id: impl Into<String>) -> Self {
        Self {
            folder_id: None,
            file_id: Some(file_id.into()),
            recursive: false,
            allow: HashSet::new(),
            deny: HashSet::new(),
        }
    }

    /// Sets `recursive`, for builder-style construction in tests and
    /// callers assembling rules programmatically.
    #[must_use]
    pub fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    /// Sets the `allow` set.
    #[must_use]
    pub fn allowing(mut self, ops: impl IntoIterator<Item = DriveOperation>) -> Self {
        self.allow = ops.into_iter().collect();
        self
    }

    /// Sets the `deny` set.
    #[must_use]
    pub fn denying(mut self, ops: impl IntoIterator<Item = DriveOperation>) -> Self {
        self.deny = ops.into_iter().collect();
        self
    }
}

/// The wire shape [`FolderPermissionRule`] deserializes through, so that
/// "exactly one of `folder_id`/`file_id`" is a **load error** rather than a
/// rule that silently matches nothing.
///
/// This is also the backward-compatibility mechanism. An older binary
/// reading a config containing a file rule sees `folder_id` missing from a
/// required field and fails `Settings::load()` outright; `active_account_rules`
/// then degrades to an empty rule set, which denies every write. That is
/// blunt — it drops the account's credentials too — but it is the same
/// fail-closed path [ADR-0073](../../docs/adrs/adr-0073.md) §3 already
/// relies on for an unrecognised `DriveOperation`, and the alternative
/// (a second, separately-named rule list) would be *silently ignored* by
/// an older binary, dropping a file-level `deny` and letting a folder-level
/// `allow` win.
#[derive(Debug, Deserialize)]
struct RawPermissionRule {
    #[serde(default)]
    folder_id: Option<String>,
    #[serde(default)]
    file_id: Option<String>,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    allow: HashSet<DriveOperation>,
    #[serde(default)]
    deny: HashSet<DriveOperation>,
}

impl TryFrom<RawPermissionRule> for FolderPermissionRule {
    type Error = String;

    fn try_from(raw: RawPermissionRule) -> Result<Self, Self::Error> {
        match (&raw.folder_id, &raw.file_id) {
            (None, None) => {
                return Err(
                    "a write-permission rule must set either `folder_id` or `file_id`".to_string(),
                )
            }
            (Some(_), Some(_)) => {
                return Err(
                    "a write-permission rule must set `folder_id` or `file_id`, not both \
                            — a rule keys on one target"
                        .to_string(),
                )
            }
            _ => {}
        }
        if raw.recursive && raw.file_id.is_some() {
            return Err(
                "`recursive` is meaningless on a `file_id` rule — a file has no descendants; \
                 drop it, or use `folder_id` to grant a whole subtree"
                    .to_string(),
            );
        }
        Ok(Self {
            folder_id: raw.folder_id,
            file_id: raw.file_id,
            recursive: raw.recursive,
            allow: raw.allow,
            deny: raw.deny,
        })
    }
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
/// `None` — the `Option` wrapping this, not a variant here — means no rule
/// matched anywhere and the bare default policy decided it instead.
/// Carried into the request log's `decided_by_folder_id`/
/// `decided_by_file_id`/`decided_by_depth` context fields so a refusal is
/// exactly as auditable as a success.
///
/// An enum rather than one struct with optional fields: a file rule has no
/// depth (it names the target itself, at depth −1) and a folder rule has no
/// file id, so "a file rule at depth 3" is a state that should not be
/// expressible at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DecidingRule {
    /// A `folder_id` rule matched the target's ancestor chain.
    Folder {
        /// The folder id of the rule that decided the verdict.
        folder_id: String,
        /// How many levels above the target this rule's folder sits (0 =
        /// the target itself).
        depth: usize,
    },
    /// A `file_id` rule named the target itself.
    File {
        /// The file id of the rule that decided the verdict.
        file_id: String,
    },
}

impl DecidingRule {
    /// The id this rule keys on — a file id or a folder id.
    ///
    /// Deliberately *not* rendered here: every caller is a CLI surface
    /// that must run the id through `sanitize_for_terminal` first, and
    /// this module stays free of the CLI layer.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Folder { folder_id, .. } => folder_id,
            Self::File { file_id } => file_id,
        }
    }

    /// `"folder"` or `"file"` — the noun the operator-facing messages use.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::Folder { .. } => "folder",
            Self::File { .. } => "file",
        }
    }

    /// `" (depth 2)"` for a folder rule, `""` for a file rule.
    ///
    /// Exists so the six `describe`/`print_report` sites are one identical
    /// line — `"refused by rule on {kind} {id}{suffix}"` — and cannot drift
    /// into disagreeing about how a file rule is worded.
    #[must_use]
    pub fn depth_suffix(&self) -> String {
        match self {
            Self::Folder { depth, .. } => format!(" (depth {depth})"),
            Self::File { .. } => String::new(),
        }
    }
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

/// The verdict from a rule naming `file_id` itself, or `None` when no
/// configured rule names it.
///
/// A file rule sits at **depth −1**: strictly more specific than any
/// folder rule, including one on the file's own immediate parent. That
/// is what lets a file shared by link or email — whose parents this
/// account cannot see at all — be granted, and it is also why a decisive
/// file rule short-circuits the ancestor walk entirely
/// (`crate::drive::folder_ancestry::resolve_decision_for_file_target`), sparing
/// every `files.get` the walk would have cost.
///
/// Deny still beats allow among file rules naming the same id, the same
/// fail-closed direction every other tie-break in this module takes.
#[must_use]
pub fn resolve_file_rule(
    file_id: &str,
    op: DriveOperation,
    rules: &[FolderPermissionRule],
) -> Option<Decision> {
    let mut deny_here = false;
    let mut allow_here = false;
    for rule in rules {
        if rule.file_id.as_deref() != Some(file_id) {
            continue;
        }
        deny_here |= rule.deny.contains(&op);
        allow_here |= rule.allow.contains(&op);
    }
    if !deny_here && !allow_here {
        return None;
    }
    Some(Decision {
        verdict: if deny_here {
            Verdict::Deny
        } else {
            Verdict::Allow
        },
        decided_by: Some(DecidingRule::File {
            file_id: file_id.to_string(),
        }),
    })
}

/// Resolves `op` for a **file** target: a rule naming `file_id` if one
/// exists, else [`resolve`] against the file's ancestor `chain`.
///
/// The pure counterpart of
/// `crate::drive::folder_ancestry::resolve_decision_for_file_target`. Callers
/// whose target is a *folder* (`create`/`upload`'s `--parent`) call
/// [`resolve`] directly instead — there is no file id to name, so a file
/// rule can never participate.
#[must_use]
pub fn resolve_for_file(
    file_id: &str,
    chain: &[String],
    op: DriveOperation,
    rules: &[FolderPermissionRule],
) -> Decision {
    resolve_file_rule(file_id, op, rules).unwrap_or_else(|| resolve(chain, op, rules))
}

/// Resolves whether `op` is permitted against `chain` (depth 0 = the
/// target folder itself, then parent, grandparent, ...) under `rules`. See
/// the module doc for the full algorithm and its tie-breaks.
///
/// Only `folder_id` rules participate: a `file_id` rule names a target,
/// not an ancestor, so it can never match a chain entry. See
/// [`resolve_for_file`] for a file target.
#[must_use]
pub fn resolve(chain: &[String], op: DriveOperation, rules: &[FolderPermissionRule]) -> Decision {
    let mut best: Option<(usize, Verdict)> = None;
    for (depth, folder_id) in chain.iter().enumerate() {
        let mut deny_here = false;
        let mut allow_here = false;
        for rule in rules {
            if rule.folder_id.as_deref() != Some(folder_id.as_str()) {
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
            decided_by: Some(DecidingRule::Folder {
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

/// The `(folder_id, file_id, depth)` triple
/// `crate::request_log::DriveMutationOutcome`'s `decided_by_folder_id`/
/// `decided_by_file_id`/`decided_by_depth` fields expect.
///
/// A named struct rather than a tuple: three same-shaped `Option`s in a
/// row is exactly the signature a caller silently mis-orders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecidedByLogFields {
    /// The deciding folder rule's id, when a folder rule decided it.
    pub folder_id: Option<String>,
    /// The deciding file rule's id, when a file rule decided it.
    pub file_id: Option<String>,
    /// The deciding folder rule's depth. Always `None` for a file rule,
    /// which matches the target itself rather than an ancestor.
    pub depth: Option<usize>,
}

/// Splits an optional [`DecidingRule`] into the log's flat fields.
///
/// Shared by `create`/`upload`/`edit`/`sheets write`'s otherwise
/// near-identical `record_attempt` functions — each still builds its own
/// `DriveMutationOutcome` (the verb-specific fields genuinely differ), but
/// this was the one piece of extraction logic that was byte-for-byte the
/// same in all of them. Kept here (rather than in `crate::request_log`,
/// which stays decoupled from any one integration's internal types) and
/// pure, matching this module's zero-I/O contract — it does not itself
/// call `crate::request_log::record_drive_mutation`.
#[must_use]
pub fn decided_by_log_fields(decided_by: Option<&DecidingRule>) -> DecidedByLogFields {
    match decided_by {
        Some(DecidingRule::Folder { folder_id, depth }) => DecidedByLogFields {
            folder_id: Some(folder_id.clone()),
            file_id: None,
            depth: Some(*depth),
        },
        Some(DecidingRule::File { file_id }) => DecidedByLogFields {
            folder_id: None,
            file_id: Some(file_id.clone()),
            depth: None,
        },
        None => DecidedByLogFields::default(),
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
        FolderPermissionRule::folder(folder_id)
            .recursive(recursive)
            .allowing(allow.iter().copied())
            .denying(deny.iter().copied())
    }

    fn file_rule(
        file_id: &str,
        allow: &[DriveOperation],
        deny: &[DriveOperation],
    ) -> FolderPermissionRule {
        FolderPermissionRule::file(file_id)
            .allowing(allow.iter().copied())
            .denying(deny.iter().copied())
    }

    fn folder_id_of(decision: &Decision) -> &str {
        match decision.decided_by.as_ref().unwrap() {
            DecidingRule::Folder { folder_id, .. } => folder_id,
            DecidingRule::File { .. } => panic!("expected a folder rule, got a file rule"),
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
        assert_eq!(folder_id_of(&decision), "root");
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
        assert_eq!(folder_id_of(&decision), "child");
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
        assert_eq!(folder_id_of(&decision), "child");
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
    fn decided_by_log_fields_none_yields_all_none() {
        assert_eq!(decided_by_log_fields(None), DecidedByLogFields::default());
    }

    #[test]
    fn decided_by_log_fields_some_extracts_folder_id_and_depth() {
        let rule = DecidingRule::Folder {
            folder_id: "folder-1".to_string(),
            depth: 2,
        };
        assert_eq!(
            decided_by_log_fields(Some(&rule)),
            DecidedByLogFields {
                folder_id: Some("folder-1".to_string()),
                file_id: None,
                depth: Some(2),
            }
        );
    }

    #[test]
    fn decided_by_log_fields_never_puts_a_file_id_in_the_folder_field() {
        // `omni-dev log --query decided_by_folder_id:X` must never start
        // matching file ids — that would silently reinterpret an existing
        // audit key.
        let rule = DecidingRule::File {
            file_id: "file-1".to_string(),
        };
        assert_eq!(
            decided_by_log_fields(Some(&rule)),
            DecidedByLogFields {
                folder_id: None,
                file_id: Some("file-1".to_string()),
                depth: None,
            }
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

    // ── file rules (issue #1612) ──────────────────────────────────────

    #[test]
    fn a_file_rule_grants_a_target_with_no_chain_at_all() {
        // The whole point of the feature: a Sheet shared by link or email
        // has no visible parent, so the chain is empty and no folder rule
        // could ever apply.
        let rules = [file_rule("shared", &[DriveOperation::SheetsWrite], &[])];
        let decision = resolve_for_file("shared", &[], DriveOperation::SheetsWrite, &rules);
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(
            decision.decided_by,
            Some(DecidingRule::File {
                file_id: "shared".to_string()
            })
        );
    }

    #[test]
    fn file_deny_beats_a_recursive_folder_allow() {
        let rules = [
            rule("root", true, &[DriveOperation::Edit], &[]),
            file_rule("target", &[], &[DriveOperation::Edit]),
        ];
        let decision = resolve_for_file(
            "target",
            &chain(&["parent", "root"]),
            DriveOperation::Edit,
            &rules,
        );
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.decided_by.unwrap().kind_label(), "file");
    }

    #[test]
    fn file_allow_beats_a_folder_deny_on_the_immediate_parent() {
        // The inverse direction — proves depth −1 is "closest wins", not a
        // one-way "a file rule can only restrict" rule.
        let rules = [
            rule("parent", false, &[], &[DriveOperation::SheetsWrite]),
            file_rule("target", &[DriveOperation::SheetsWrite], &[]),
        ];
        let decision = resolve_for_file(
            "target",
            &chain(&["parent"]),
            DriveOperation::SheetsWrite,
            &rules,
        );
        assert_eq!(decision.verdict, Verdict::Allow);
    }

    #[test]
    fn deny_beats_allow_among_file_rules_for_the_same_id() {
        let rules = [
            file_rule("target", &[DriveOperation::Edit], &[]),
            file_rule("target", &[], &[DriveOperation::Edit]),
        ];
        let decision = resolve_for_file("target", &[], DriveOperation::Edit, &rules);
        assert_eq!(decision.verdict, Verdict::Deny);
    }

    #[test]
    fn resolve_file_rule_returns_none_rather_than_the_default_policy() {
        // Must be distinguishable from "decided deny", or the caller would
        // skip the ancestor walk that could still have granted it.
        let rules = [file_rule("target", &[DriveOperation::Edit], &[])];
        assert!(resolve_file_rule("target", DriveOperation::SheetsWrite, &rules).is_none());
        assert!(resolve_file_rule("other", DriveOperation::Edit, &rules).is_none());
    }

    #[test]
    fn a_file_rule_is_invisible_to_the_ancestor_walk() {
        // A `file_id` rule whose id happens to equal a folder in the chain
        // must not match it: the two id spaces are not interchangeable.
        let rules = [file_rule("target", &[DriveOperation::Create], &[])];
        let decision = resolve(&chain(&["target"]), DriveOperation::Create, &rules);
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.decided_by, None);
    }

    #[test]
    fn a_folder_rule_is_invisible_to_the_file_lookup() {
        let rules = [rule("target", true, &[DriveOperation::Create], &[])];
        assert!(resolve_file_rule("target", DriveOperation::Create, &rules).is_none());
    }

    #[test]
    fn resolve_for_file_falls_through_to_the_chain_when_no_file_rule_matches() {
        let rules = [rule("parent", true, &[DriveOperation::Edit], &[])];
        let decision =
            resolve_for_file("target", &chain(&["parent"]), DriveOperation::Edit, &rules);
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(folder_id_of(&decision), "parent");
    }

    // ── rule schema validation ────────────────────────────────────────

    fn parse_rule(json: &str) -> Result<FolderPermissionRule, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn a_folder_rule_round_trips_byte_identically_to_the_pre_1612_shape() {
        let parsed =
            parse_rule(r#"{"folder_id":"f1","recursive":true,"allow":["create"]}"#).unwrap();
        assert_eq!(parsed.folder_id.as_deref(), Some("f1"));
        assert_eq!(parsed.file_id, None);
        assert!(parsed.recursive);
        let json: serde_json::Value = serde_json::to_value(&parsed).unwrap();
        assert_eq!(json["folder_id"], "f1");
        assert_eq!(json["recursive"], true);
        assert!(
            json.get("file_id").is_none(),
            "an absent file_id must not be serialized"
        );
    }

    #[test]
    fn a_file_rule_parses_and_serializes_without_a_folder_id() {
        let parsed = parse_rule(r#"{"file_id":"x1","allow":["sheets-write"]}"#).unwrap();
        assert_eq!(parsed.file_id.as_deref(), Some("x1"));
        assert_eq!(parsed.folder_id, None);
        let json: serde_json::Value = serde_json::to_value(&parsed).unwrap();
        assert_eq!(json["file_id"], "x1");
        assert!(json.get("folder_id").is_none());
    }

    #[test]
    fn a_rule_naming_neither_target_is_a_load_error() {
        let err = parse_rule(r#"{"allow":["create"]}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("folder_id"), "{err}");
        assert!(err.contains("file_id"), "{err}");
    }

    #[test]
    fn a_rule_naming_both_targets_is_a_load_error() {
        let err = parse_rule(r#"{"folder_id":"f","file_id":"x"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn recursive_true_on_a_file_rule_is_a_load_error() {
        // Silently ignoring it would let an operator believe they had
        // granted a subtree when they had granted one file.
        let err = parse_rule(r#"{"file_id":"x","recursive":true}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("recursive"), "{err}");
    }

    #[test]
    fn recursive_false_on_a_file_rule_is_accepted() {
        // Redundant but not a misunderstanding, so it must not error.
        let parsed = parse_rule(r#"{"file_id":"x","recursive":false}"#).unwrap();
        assert!(!parsed.recursive);
    }

    #[test]
    fn deciding_rule_accessors_render_both_kinds() {
        let folder = DecidingRule::Folder {
            folder_id: "f1".to_string(),
            depth: 2,
        };
        assert_eq!(folder.id(), "f1");
        assert_eq!(folder.kind_label(), "folder");
        assert_eq!(folder.depth_suffix(), " (depth 2)");

        let file = DecidingRule::File {
            file_id: "x1".to_string(),
        };
        assert_eq!(file.id(), "x1");
        assert_eq!(file.kind_label(), "file");
        assert_eq!(file.depth_suffix(), "");
    }
}
