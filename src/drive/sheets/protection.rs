//! Protected ranges via `spreadsheets.batchUpdate` (issue #1643,
//! [ADR-0077](../../../docs/adrs/adr-0077.md)).
//!
//! Gated by [`DriveOperation::SheetsProtection`], **not**
//! `SheetsStructure` — see that variant's doc comment for why. A protected
//! range is a permission change inside the document (who may edit, not
//! what the sheet contains), which is a different category from every
//! other verb this issue ships.
//!
//! Three mutating verbs (`protect-range`/`update-protection`/
//! `unprotect-range`) plus one read (`list-protections`, ungated like
//! `sheets info`). Shape mirrors `format.rs`/`validation.rs`: compose a
//! target, resolve it against a freshly-fetched workbook, gate, dry-run,
//! mutate, log.
//!
//! **`update-protection`/`unprotect-range` resolve their target by exact
//! range match against the workbook's current protected ranges — never a
//! guess.** Sheets has no other stable handle a CLI user could type (the
//! numeric `protectedRangeId` is discoverable only via `list-protections`,
//! which is the point of shipping that command at all); an ambiguous or
//! absent match is refused, the same "refuses to guess" posture
//! `geometry.rs`'s title-matching cascade documents.
//!
//! **Sheets has no incremental editor add/remove.**
//! `updateProtectedRange`'s `editors` field replaces the whole list, so
//! `update-protection --add-editor`/`--remove-editor` compute the full
//! resulting set from the range's *current* editors (read in the same
//! fetch used to resolve the target) before sending it.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::sheets::a1;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AddProtectedRangeRequest, BatchUpdateRequestItem, DeleteProtectedRangeRequest, GridRange,
    ProtectedRange, ProtectedRangeEditors, ProtectedRangeUpdate, Spreadsheet,
    UpdateProtectedRangeRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectionVerb {
    /// Protect a range (or, with `whole_sheet`, an entire sheet).
    ProtectRange {
        /// A sheet title, supplying a prefix for a bare `range`. Also the
        /// target sheet directly when `whole_sheet` is set.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        /// Mutually exclusive with `whole_sheet`.
        range: Option<String>,
        /// Protect the entire sheet named by `sheet`, rather than a range
        /// within it.
        whole_sheet: bool,
        /// A human-readable note about why the range is protected.
        description: Option<String>,
        /// `true` only warns on an edit; `false` (the default) blocks it.
        warning_only: bool,
        /// Editors exempted from the protection.
        editors: Vec<String>,
    },
    /// Change an existing protected range's description, warning-only
    /// flag, or editor list.
    UpdateProtection {
        /// A sheet title, supplying a prefix for a bare `range`. Also the
        /// target sheet directly when `whole_sheet` is set.
        sheet: Option<String>,
        /// An explicit A1 range identifying the *existing* protection to
        /// change, by exact match. Mutually exclusive with `whole_sheet`.
        range: Option<String>,
        /// Target the whole-sheet protection on `sheet`, rather than one
        /// covering a range within it — the only way to address a
        /// protection `protect-range --whole-sheet` created, since it has
        /// no range of its own to match against.
        whole_sheet: bool,
        /// The new description, when changing it.
        description: Option<String>,
        /// The new warning-only flag, when changing it.
        warning_only: Option<bool>,
        /// Editors to add.
        add_editors: Vec<String>,
        /// Editors to remove.
        remove_editors: Vec<String>,
    },
    /// Remove a protected range.
    UnprotectRange {
        /// A sheet title, supplying a prefix for a bare `range`. Also the
        /// target sheet directly when `whole_sheet` is set.
        sheet: Option<String>,
        /// An explicit A1 range identifying the protection to remove, by
        /// exact match. Mutually exclusive with `whole_sheet`.
        range: Option<String>,
        /// Target the whole-sheet protection on `sheet` — see
        /// [`Self::UpdateProtection::whole_sheet`].
        whole_sheet: bool,
    },
}

impl ProtectionVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::ProtectRange { .. } => "sheets-protect-range",
            Self::UpdateProtection { .. } => "sheets-update-protection",
            Self::UnprotectRange { .. } => "sheets-unprotect-range",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::ProtectRange { .. } => "protect-range",
            Self::UpdateProtection { .. } => "update-protection",
            Self::UnprotectRange { .. } => "unprotect-range",
        }
    }

    /// `--sheet`, `--range`, and whether `--whole-sheet` was given — every
    /// verb has all three, just not always under that name (`ProtectRange`
    /// calls it the same thing).
    fn sheet_range_whole(&self) -> (Option<&str>, Option<&str>, bool) {
        match self {
            Self::ProtectRange {
                sheet,
                range,
                whole_sheet,
                ..
            }
            | Self::UpdateProtection {
                sheet,
                range,
                whole_sheet,
                ..
            }
            | Self::UnprotectRange {
                sheet,
                range,
                whole_sheet,
            } => (sheet.as_deref(), range.as_deref(), *whole_sheet),
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct ProtectionOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: ProtectionVerb,
    /// Classify and describe only; never call `batchUpdate`.
    pub dry_run: bool,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ProtectionResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// A human-readable summary of the effect.
        summary: String,
    },
    /// The target is not a Google Sheet.
    RefusedNotASpreadsheet {
        /// The target's actual MIME type.
        mime_type: String,
    },
    /// The target is a shortcut, which we do not follow.
    RefusedShortcut,
    /// The target has no parents this account can see.
    RefusedNoVisibleParents,
    /// The named sheet does not exist in this workbook.
    RefusedSheetNotFound {
        /// The title that was not found.
        title: String,
        /// The titles that do exist.
        available: Vec<String>,
    },
    /// The `--sheet`/`--range` pair was invalid.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `update-protection`/`unprotect-range` found no protected range
    /// covering exactly the given target.
    RefusedProtectionNotFound {
        /// What was searched for.
        detail: String,
    },
    /// `update-protection`/`unprotect-range`'s target matched more than one
    /// protected range — refused rather than guessing which was meant.
    RefusedAmbiguousProtection {
        /// The matching ids, so the user can disambiguate via
        /// `list-protections`.
        candidates: Vec<i64>,
    },
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any.
        decided_by: Option<DecidingRule>,
    },
    /// The mutation succeeded.
    Changed {
        /// Same summary as [`Self::WouldChange`].
        summary: String,
        /// The protected range's stable id — server-assigned for
        /// `protect-range`, otherwise the one resolved against.
        protected_range_id: Option<i64>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl ProtectionResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedProtectionNotFound { .. } => "refused-protection-not-found",
            Self::RefusedAmbiguousProtection { .. } => "refused-ambiguous-protection",
            Self::Blocked { .. } => "blocked",
            Self::Changed { .. } => "changed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProtectionOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// Which mutation was attempted. Not serialised.
    #[serde(skip)]
    pub verb: ProtectionVerb,
    /// What happened.
    pub result: ProtectionResult,
}

impl JsonlSerialize for ProtectionOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one protection mutation, logging every attempt that isn't a dry
/// run.
pub async fn protection(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &ProtectionOptions,
    rules: &[FolderPermissionRule],
) -> ProtectionOutcome {
    let started = Instant::now();
    let outcome = protection_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn protection_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &ProtectionOptions,
    rules: &[FolderPermissionRule],
) -> ProtectionOutcome {
    let bare = |result| ProtectionOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        verb: opts.verb.clone(),
        result,
    };

    let composed_range = match compose_target(&opts.verb) {
        Ok(composed) => composed,
        Err(detail) => return bare(ProtectionResult::RefusedInvalidRange { detail }),
    };

    if let Err(detail) = validate_verb(&opts.verb) {
        return bare(ProtectionResult::RefusedInvalidRange { detail });
    }

    let (target, decision, resolved_folder_id) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsProtection,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(ProtectionResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => ProtectionResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    ProtectionResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => ProtectionResult::RefusedNoVisibleParents,
            };
            return ProtectionOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return ProtectionOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result: ProtectionResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
        } => (target, decision, resolved_folder_id),
    };

    let gated = |result| ProtectionOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(ProtectionResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api
        .get_spreadsheet_with_protections(&opts.spreadsheet_id)
        .await
    {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(ProtectionResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let grid = match resolve_grid(&workbook, &opts.verb, composed_range.as_deref()) {
        Ok(grid) => grid,
        Err(result) => return gated(result),
    };

    // `update-protection`/`unprotect-range` resolve their target by exact
    // match against the workbook's current protections — done once, here,
    // before the dry-run check, so a preview refuses a nonexistent or
    // ambiguous target exactly like a real attempt would.
    let existing = match &opts.verb {
        ProtectionVerb::ProtectRange { .. } => None,
        ProtectionVerb::UpdateProtection { .. } | ProtectionVerb::UnprotectRange { .. } => {
            match find_existing_protection(&workbook, grid) {
                Ok(existing) => Some(existing),
                Err(result) => return gated(result),
            }
        }
    };

    let summary = describe_effect(&opts.verb);

    if opts.dry_run {
        return gated(ProtectionResult::WouldChange { summary });
    }

    // Only past the dry-run return does building the actual request (in
    // particular `update-protection`'s editor-list merge) do any work.
    let built = match &opts.verb {
        ProtectionVerb::ProtectRange {
            description,
            warning_only,
            editors,
            ..
        } => Ok((
            BatchUpdateRequestItem::AddProtectedRange(AddProtectedRangeRequest {
                protected_range: ProtectedRange {
                    protected_range_id: None,
                    range: Some(grid),
                    description: description.clone(),
                    warning_only: Some(*warning_only),
                    editors: (!editors.is_empty()).then(|| ProtectedRangeEditors {
                        users: editors.clone(),
                    }),
                },
            }),
            None,
        )),
        ProtectionVerb::UpdateProtection {
            description,
            warning_only,
            add_editors,
            remove_editors,
            ..
        } => {
            let Some(existing) = existing else {
                unreachable!("existing is resolved for UpdateProtection above")
            };
            build_update(
                existing,
                description,
                *warning_only,
                add_editors,
                remove_editors,
            )
            .map(|update| {
                (
                    BatchUpdateRequestItem::UpdateProtectedRange(update),
                    existing.protected_range_id,
                )
            })
        }
        ProtectionVerb::UnprotectRange { .. } => {
            let Some(existing) = existing else {
                unreachable!("existing is resolved for UnprotectRange above")
            };
            let id = existing.protected_range_id;
            Ok((
                BatchUpdateRequestItem::DeleteProtectedRange(DeleteProtectedRangeRequest {
                    protected_range_id: id.unwrap_or_default(),
                }),
                id,
            ))
        }
    };
    let (request, existing_id) = match built {
        Ok(built) => built,
        Err(detail) => return gated(ProtectionResult::RefusedInvalidRange { detail }),
    };

    match api.batch_update(&opts.spreadsheet_id, vec![request]).await {
        Ok(response) => {
            let protected_range_id = added_protected_range_id(&response).or(existing_id);
            gated(ProtectionResult::Changed {
                summary,
                protected_range_id,
            })
        }
        Err(err) => gated(ProtectionResult::Failed {
            detail: format!("{err:#}"),
        }),
    }
}

/// Composes the `--sheet`/`--range` pair into one string, exactly like
/// `format.rs`/`validation.rs` — except for `--whole-sheet`, which names a
/// sheet directly and needs no range composition at all. Returns `None`
/// only for that case; [`resolve_grid`] is what actually interprets it.
///
/// Shared across all three verbs: `update-protection`/`unprotect-range`
/// need `--whole-sheet` exactly as much as `protect-range` does, since it's
/// the only way to address a protection that `protect-range --whole-sheet`
/// created — such a protection has no range of its own to name.
fn compose_target(verb: &ProtectionVerb) -> Result<Option<String>, String> {
    let (sheet, range, whole_sheet) = verb.sheet_range_whole();
    if whole_sheet {
        if range.is_some() {
            return Err("--whole-sheet and --range are mutually exclusive".to_string());
        }
        if sheet.is_none() {
            return Err("--whole-sheet needs --sheet to name the sheet to protect".to_string());
        }
        return Ok(None);
    }
    a1::compose(sheet, range)
        .map(Some)
        .map_err(|err| err.to_string())
}

/// Rejects a verb whose own arguments are internally inconsistent —
/// confirmed against the live API, not guessed at: `protect-range
/// --warning-only --editor` is rejected server-side with "ProtectedRange is
/// warningOnly. Editors cannot be set on it." (a warning-only protection
/// never blocks an edit, so there is no "who may bypass the block" to
/// name). Checked here, before the gate, so the refusal is as cheap as
/// every other `RefusedInvalidRange`.
fn validate_verb(verb: &ProtectionVerb) -> Result<(), String> {
    if let ProtectionVerb::ProtectRange {
        warning_only: true,
        editors,
        ..
    } = verb
    {
        if !editors.is_empty() {
            return Err(
                "--warning-only and --editor are mutually exclusive: a warning-only \
                 protection never blocks an edit, so there is no one to exempt from it"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn resolve_grid(
    workbook: &Spreadsheet,
    verb: &ProtectionVerb,
    composed: Option<&str>,
) -> Result<GridRange, ProtectionResult> {
    // `--whole-sheet` names its sheet directly — `compose_target` returns
    // `None` for it precisely so this branch handles it without a range to
    // parse at all.
    let (sheet, _range, whole_sheet) = verb.sheet_range_whole();
    if whole_sheet {
        let Some(sheet) = sheet else {
            return Err(ProtectionResult::RefusedInvalidRange {
                detail: "--whole-sheet needs --sheet to name the sheet to protect".to_string(),
            });
        };
        let sheet_id = find_sheet_id(workbook, sheet)?;
        return Ok(GridRange {
            sheet_id,
            ..Default::default()
        });
    }
    let composed = composed.unwrap_or_default();
    let (_, grid) = grid_range::resolve_grid_range(
        workbook,
        composed,
        |detail| ProtectionResult::RefusedInvalidRange { detail },
        |title, available| ProtectionResult::RefusedSheetNotFound { title, available },
    )?;
    Ok(grid)
}

fn find_sheet_id(workbook: &Spreadsheet, title: &str) -> Result<i64, ProtectionResult> {
    grid_range::find_sheet_id(workbook, title, |title, available| {
        ProtectionResult::RefusedSheetNotFound { title, available }
    })
}

/// Finds the one existing protected range whose `range` exactly matches
/// `grid` — never a superset, subset, or overlap. Refuses rather than
/// guesses on zero or multiple matches.
fn find_existing_protection(
    workbook: &Spreadsheet,
    grid: GridRange,
) -> Result<&ProtectedRange, ProtectionResult> {
    let matches: Vec<&ProtectedRange> = workbook
        .sheets
        .iter()
        .flat_map(|sheet| sheet.protected_ranges.iter())
        .filter(|p| p.range.as_ref() == Some(&grid))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(ProtectionResult::RefusedProtectionNotFound {
            detail: "no protected range exactly matches this target; run \
                     `drive sheets list-protections` to see what exists"
                .to_string(),
        }),
        many => Err(ProtectionResult::RefusedAmbiguousProtection {
            candidates: many.iter().filter_map(|p| p.protected_range_id).collect(),
        }),
    }
}

/// Builds the `updateProtectedRange` request, from the existing
/// protection's current editors and the requested add/remove sets — the
/// full resulting list, since Sheets has no incremental add/remove (see
/// the module docs).
fn build_update(
    existing: &ProtectedRange,
    description: &Option<String>,
    warning_only: Option<bool>,
    add_editors: &[String],
    remove_editors: &[String],
) -> Result<UpdateProtectedRangeRequest, String> {
    let mut fields = Vec::new();
    let mut update = ProtectedRangeUpdate {
        protected_range_id: existing.protected_range_id.unwrap_or_default(),
        ..Default::default()
    };
    if let Some(description) = description {
        update.description = Some(description.clone());
        fields.push("description");
    }
    if let Some(warning_only) = warning_only {
        update.warning_only = Some(warning_only);
        fields.push("warningOnly");
    }
    let mut resulting_editors: Vec<String> = existing
        .editors
        .as_ref()
        .map(|e| e.users.clone())
        .unwrap_or_default();
    if !add_editors.is_empty() || !remove_editors.is_empty() {
        for editor in add_editors {
            if !resulting_editors.contains(editor) {
                resulting_editors.push(editor.clone());
            }
        }
        resulting_editors.retain(|e| !remove_editors.contains(e));
        update.editors = Some(ProtectedRangeEditors {
            users: resulting_editors.clone(),
        });
        fields.push("editors");
    }
    // The same server constraint `validate_verb` checks for `protect-range`
    // — confirmed against the live API — applies here too, except the
    // *resulting* state can come from either side: a target already
    // warning-only that gains an editor, or a strict target whose editors
    // survive a switch to warning-only. Both combine `existing`'s state
    // with this call's overrides, which is why this check lives here
    // rather than in `validate_verb`, which only ever sees the request.
    let resulting_warning_only =
        warning_only.unwrap_or_else(|| existing.warning_only.unwrap_or(false));
    if resulting_warning_only && !resulting_editors.is_empty() {
        return Err(
            "this would leave the protection warning-only with editors set, which Sheets \
             rejects: a warning-only protection never blocks an edit, so there is no one to \
             exempt from it — drop --warning-only or remove every editor first"
                .to_string(),
        );
    }
    Ok(UpdateProtectedRangeRequest {
        protected_range: update,
        fields: fields.join(","),
    })
}

fn added_protected_range_id(
    response: &crate::drive::sheets::types::BatchUpdateResponse,
) -> Option<i64> {
    response
        .replies
        .iter()
        .find_map(|reply| reply.add_protected_range.as_ref())
        .and_then(|added| added.protected_range.as_ref())
        .and_then(|range| range.protected_range_id)
}

fn describe_effect(verb: &ProtectionVerb) -> String {
    match verb {
        ProtectionVerb::ProtectRange {
            warning_only,
            editors,
            ..
        } => {
            let strictness = if *warning_only {
                "warn only"
            } else {
                "block edits"
            };
            let editors = if editors.is_empty() {
                String::new()
            } else {
                format!(", editors exempted: {}", editors.join(", "))
            };
            format!("protect ({strictness}{editors})")
        }
        ProtectionVerb::UpdateProtection {
            add_editors,
            remove_editors,
            ..
        } => {
            let mut parts = Vec::new();
            if !add_editors.is_empty() {
                parts.push(format!("+{}", add_editors.join(",")));
            }
            if !remove_editors.is_empty() {
                parts.push(format!("-{}", remove_editors.join(",")));
            }
            if parts.is_empty() {
                "update protection".to_string()
            } else {
                format!("update protection ({})", parts.join(" "))
            }
        }
        ProtectionVerb::UnprotectRange { .. } => "remove protection".to_string(),
    }
}

fn record_attempt(outcome: &ProtectionOutcome, opts: &ProtectionOptions, duration: Duration) {
    let error = match &outcome.result {
        ProtectionResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        ProtectionResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    // Editor changes are only real once the outcome is `Changed` — a
    // `Blocked`/`Failed`/refused attempt granted or revoked nothing, so
    // logging the *requested* editors there (as opposed to what the verb
    // carries) would misrepresent the audit trail for a permission surface
    // where that record matters.
    let (protected_range_id, editors_added, editors_removed) = match &outcome.result {
        ProtectionResult::Changed {
            protected_range_id, ..
        } => {
            let (added, removed) = match &opts.verb {
                ProtectionVerb::ProtectRange { editors, .. } => (editors.clone(), Vec::new()),
                ProtectionVerb::UpdateProtection {
                    add_editors,
                    remove_editors,
                    ..
                } => (add_editors.clone(), remove_editors.clone()),
                ProtectionVerb::UnprotectRange { .. } => (Vec::new(), Vec::new()),
            };
            (*protected_range_id, added, removed)
        }
        _ => (None, Vec::new(), Vec::new()),
    };

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: opts.verb.log_operation(),
        file_id: outcome.spreadsheet_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
        protected_range_id,
        protection_editors_added: editors_added,
        protection_editors_removed: editors_removed,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &ProtectionOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &ProtectionOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        ProtectionResult::WouldChange { summary } => vec![format!("Would {summary} in {book}")],
        ProtectionResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        ProtectionResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        ProtectionResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-protection\"]}} to write_permissions.rules."
        )],
        ProtectionResult::RefusedSheetNotFound { title, available } => {
            let list = if available.is_empty() {
                "none".to_string()
            } else {
                available
                    .iter()
                    .map(|t| format!("'{t}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            vec![format!(
                "Refused: {book} has no sheet titled '{title}'. Available: {list}"
            )]
        }
        ProtectionResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        ProtectionResult::RefusedProtectionNotFound { detail } => {
            vec![format!("Refused: {detail}")]
        }
        ProtectionResult::RefusedAmbiguousProtection { candidates } => vec![format!(
            "Refused: more than one protected range matches this target (ids: {}); \
             this is ambiguous and nothing was changed",
            candidates
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )],
        ProtectionResult::Blocked { decided_by } => vec![match decided_by {
            Some(rule) => format!(
                "Blocked: {} on {book} refused by rule on {} {}{}",
                verb.label(),
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: {} on {book} refused by default policy (no matching rule for \
                 sheets-protection)",
                verb.label()
            ),
        }],
        ProtectionResult::Changed {
            summary,
            protected_range_id,
        } => {
            let id = protected_range_id.map_or_else(String::new, |id| format!(" (id {id})"));
            vec![format!("Applied: {summary}{id} in {book}")]
        }
        ProtectionResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::Sheet;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    fn grid(sheet_id: i64) -> GridRange {
        GridRange {
            sheet_id,
            start_row_index: Some(0),
            end_row_index: Some(5),
            start_column_index: Some(0),
            end_column_index: Some(1),
        }
    }

    fn protected(id: i64, range: GridRange) -> ProtectedRange {
        ProtectedRange {
            protected_range_id: Some(id),
            range: Some(range),
            ..Default::default()
        }
    }

    fn workbook_with(protections: Vec<ProtectedRange>) -> Spreadsheet {
        Spreadsheet {
            sheets: vec![Sheet {
                properties: None,
                protected_ranges: protections,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn find_existing_protection_matches_by_exact_range() {
        let workbook = workbook_with(vec![protected(10, grid(1))]);
        let found = find_existing_protection(&workbook, grid(1)).unwrap();
        assert_eq!(found.protected_range_id, Some(10));
    }

    #[test]
    fn find_existing_protection_refuses_when_none_matches() {
        let workbook = workbook_with(vec![protected(10, grid(1))]);
        let mut other = grid(1);
        other.end_row_index = Some(6);
        let err = find_existing_protection(&workbook, other).unwrap_err();
        assert!(matches!(
            err,
            ProtectionResult::RefusedProtectionNotFound { .. }
        ));
    }

    #[test]
    fn find_existing_protection_refuses_when_ambiguous() {
        let workbook = workbook_with(vec![protected(10, grid(1)), protected(11, grid(1))]);
        let err = find_existing_protection(&workbook, grid(1)).unwrap_err();
        match err {
            ProtectionResult::RefusedAmbiguousProtection { mut candidates } => {
                candidates.sort_unstable();
                assert_eq!(candidates, vec![10, 11]);
            }
            other => panic!("expected RefusedAmbiguousProtection, got {other:?}"),
        }
    }

    #[test]
    fn build_update_adds_and_removes_editors_from_the_current_set() {
        let existing = ProtectedRange {
            protected_range_id: Some(1),
            editors: Some(ProtectedRangeEditors {
                users: vec!["a@example.com".to_string(), "b@example.com".to_string()],
            }),
            ..Default::default()
        };
        let request = build_update(
            &existing,
            &None,
            None,
            &["c@example.com".to_string()],
            &["a@example.com".to_string()],
        )
        .unwrap();
        let mut users = request.protected_range.editors.unwrap().users;
        users.sort();
        assert_eq!(users, vec!["b@example.com", "c@example.com"]);
        assert_eq!(request.fields, "editors");
    }

    #[test]
    fn build_update_leaves_editors_untouched_when_neither_add_nor_remove_given() {
        let existing = ProtectedRange {
            protected_range_id: Some(1),
            editors: Some(ProtectedRangeEditors {
                users: vec!["a@example.com".to_string()],
            }),
            ..Default::default()
        };
        let request = build_update(&existing, &Some("note".to_string()), None, &[], &[]).unwrap();
        assert!(request.protected_range.editors.is_none());
        assert_eq!(request.fields, "description");
    }

    #[test]
    fn build_update_refuses_warning_only_with_editors_from_either_side() {
        // The existing protection already carries an editor; switching it
        // to warning-only must be refused rather than sent to a server that
        // will reject it anyway.
        let existing = ProtectedRange {
            protected_range_id: Some(1),
            editors: Some(ProtectedRangeEditors {
                users: vec!["a@example.com".to_string()],
            }),
            ..Default::default()
        };
        let err = build_update(&existing, &None, Some(true), &[], &[]).unwrap_err();
        assert!(err.contains("warning-only"), "{err}");

        // The existing protection is already warning-only; adding an
        // editor must be refused the same way, even with `warning_only`
        // left untouched by this call.
        let existing_warning_only = ProtectedRange {
            protected_range_id: Some(1),
            warning_only: Some(true),
            ..Default::default()
        };
        let err = build_update(
            &existing_warning_only,
            &None,
            None,
            &["a@example.com".to_string()],
            &[],
        )
        .unwrap_err();
        assert!(err.contains("warning-only"), "{err}");
    }

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn clients(server: &wiremock::MockServer) -> (DriveClient, SheetsClient) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token", "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;
        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(SHEETS_API_URL, &server.uri());
        let sheets = SheetsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, sheets)
    }

    fn mount_file(id: &str, mime_type: &str, parents: &[&str]) -> wiremock::Mock {
        let parents: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": mime_type, "parents": parents,
                })),
            )
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        mount_file(id, "application/vnd.google-apps.folder", &[])
    }

    fn allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsProtection).collect(),
            deny: HashSet::default(),
        }
    }

    fn mount_workbook(protected_ranges: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0},
                         "protectedRanges": protected_ranges},
                    ],
                })),
            )
    }

    #[tokio::test]
    async fn a_denied_gate_blocks_before_any_read_or_batch_update_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::ProtectRange {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
                description: None,
                warning_only: false,
                editors: Vec::new(),
            },
            dry_run: false,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn protect_range_sends_an_add_protected_range_request() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([])).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{"addProtectedRange": {"protectedRange": {"protectedRangeId": 99}}}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::ProtectRange {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
                description: Some("locked".to_string()),
                warning_only: false,
                editors: Vec::new(),
            },
            dry_run: false,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ProtectionResult::Changed {
                protected_range_id, ..
            } => assert_eq!(protected_range_id, Some(99)),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unprotect_range_refuses_when_no_protection_matches() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([])).mount(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::UnprotectRange {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
            },
            dry_run: false,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::RefusedProtectionNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn unprotect_range_whole_sheet_finds_a_whole_sheet_protection() {
        // A whole-sheet protection has no range of its own — `--whole-sheet`
        // on `unprotect-range` is the only way to target it at all (issue
        // #1643 follow-up: previously unreachable once created).
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([{
            "protectedRangeId": 42,
            "range": {"sheetId": 0},
        }]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::UnprotectRange {
                sheet: Some("Q1".to_string()),
                range: None,
                whole_sheet: true,
            },
            dry_run: false,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ProtectionResult::Changed {
                protected_range_id, ..
            } => assert_eq!(protected_range_id, Some(42)),
            other => panic!("expected Changed, got {other:?}"),
        }
    }
}
