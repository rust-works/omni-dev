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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::check::{
    check_and_lock_lease, finish_leased_native_write, record_failed_leased_write,
    LeaseCheckOutcome, LeasedWrite,
};
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
    /// The lease token presented via `--lease`. Checked only when the
    /// deciding rule requires one
    /// ([`write_gate::decided_rule_requires_lease`], ADR-0080 §1/§9);
    /// `None` is only ever valid when it does not.
    pub lease_token: Option<String>,
    /// Path to the lease ledger the token is checked against. Production
    /// callers pass `crate::drive::lease::ledger::ledger_path`'s own
    /// result; tests pass a path under a `tempdir`.
    pub ledger_path: PathBuf,
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
    /// No `--lease` was presented, and the deciding rule requires one
    /// (ADR-0080 §9).
    RefusedNoLease,
    /// The presented lease has expired, or was never a token this ledger
    /// knows about.
    RefusedLeaseExpired,
    /// The presented lease is bound to a different file id.
    RefusedLeaseWrongFile,
    /// The file has moved since the lease's recorded `version` — the
    /// staleness check (ADR-0080 §6).
    RefusedLeaseStale,
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
            Self::RefusedNoLease => "refused-no-lease",
            Self::RefusedLeaseExpired => "refused-lease-expired",
            Self::RefusedLeaseWrongFile => "refused-lease-wrong-file",
            Self::RefusedLeaseStale => "refused-lease-stale",
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

    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
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
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
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

    // The lease check (ADR-0080 §9) sits here: after the permission gate
    // and the `--dry-run` branch, before the mutating call — see
    // `content_edit.rs::edit_inner`'s doc comment for the full reasoning,
    // shared verbatim by every leased engine. A fresh `files.get` immediately
    // before `batchUpdate`, not a reuse of the metadata `target_gate::resolve`
    // fetched before the (potentially slow) ancestor-chain walk and workbook
    // fetch above.
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets protection",
        operation: opts.verb.log_operation(),
        ledger_path: &opts.ledger_path,
        file_id: &opts.spreadsheet_id,
    };
    let lease_lock = if requires_lease {
        let (live_version, live_modified_time) =
            match files_api.get_metadata(&opts.spreadsheet_id).await {
                Ok(fresh) => (fresh.version, fresh.modified_time),
                Err(err) => {
                    return gated(ProtectionResult::Failed {
                        detail: err.to_string(),
                    })
                }
            };
        match check_and_lock_lease(
            leased,
            opts.lease_token.as_deref(),
            live_version.as_deref(),
            live_modified_time.as_deref(),
        ) {
            LeaseCheckOutcome::Ok(lock) => Some(lock),
            LeaseCheckOutcome::NoLease => return gated(ProtectionResult::RefusedNoLease),
            LeaseCheckOutcome::Expired => return gated(ProtectionResult::RefusedLeaseExpired),
            LeaseCheckOutcome::WrongFile => return gated(ProtectionResult::RefusedLeaseWrongFile),
            LeaseCheckOutcome::Stale => return gated(ProtectionResult::RefusedLeaseStale),
            LeaseCheckOutcome::Failed(detail) => return gated(ProtectionResult::Failed { detail }),
        }
    } else {
        None
    };

    let result = match api.batch_update(&opts.spreadsheet_id, vec![request]).await {
        Ok(response) => {
            if let (Some(token), Some(lock)) = (&opts.lease_token, &lease_lock) {
                finish_leased_native_write(leased, lock, token, &files_api).await;
            }
            let protected_range_id = added_protected_range_id(&response).or(existing_id);
            ProtectionResult::Changed {
                summary,
                protected_range_id,
            }
        }
        Err(err) => {
            let detail = format!("{err:#}");
            if let (Some(token), Some(_lock)) = (&opts.lease_token, &lease_lock) {
                record_failed_leased_write(leased, token, &detail);
            }
            ProtectionResult::Failed { detail }
        }
    };
    drop(lease_lock);
    gated(result)
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
        ProtectionResult::RefusedNoLease => vec![format!(
            "Refused: {book} requires a Drive write lease — run `omni-dev drive lease acquire \
             {}` and pass the printed token via `--lease`.",
            outcome.spreadsheet_id
        )],
        ProtectionResult::RefusedLeaseExpired => vec![format!(
            "Refused: the presented lease is expired, released, or unknown to this ledger — \
             run `omni-dev drive lease acquire {}` again.",
            outcome.spreadsheet_id
        )],
        ProtectionResult::RefusedLeaseWrongFile => vec![format!(
            "Refused: the presented lease was acquired for a different file — run `omni-dev \
             drive lease acquire {}` for this one.",
            outcome.spreadsheet_id
        )],
        ProtectionResult::RefusedLeaseStale => vec![format!(
            "Refused: {book} changed since the lease was acquired (or last written under) — \
             re-run `omni-dev drive lease acquire {}` to lease the current version.",
            outcome.spreadsheet_id
        )],
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
    use crate::drive::sheets::types::{Sheet, SheetProperties};
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

    /// `version: "1"` throughout — matches [`leased_opts_for`]'s default
    /// seeded lease, so any test reaching the mutating call has a live,
    /// non-stale lease by construction (ADR-0080 §9).
    fn mount_file(id: &str, mime_type: &str, parents: &[&str]) -> wiremock::Mock {
        let parents: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": mime_type, "parents": parents,
                    "version": "1",
                })),
            )
    }

    /// Seeds `ledger_path` with a fresh, live lease for `spreadsheet_id` at
    /// `version`, returning its token.
    fn seed_lease(ledger_path: &std::path::Path, spreadsheet_id: &str, version: &str) -> String {
        // A fixed token, not a random one: every call gets its own isolated
        // ledger (a fresh tempdir), so uniqueness across tests is never a
        // concern.
        let token = "test-lease-token".to_string();
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: token.clone(),
            file_id: spreadsheet_id.to_string(),
            version: version.to_string(),
            modified_time: None,
            backup: crate::drive::lease::ledger::LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            released_at: None,
        });
        ledger.save(ledger_path).unwrap();
        token
    }

    /// A fresh, isolated ledger path holding a live lease for
    /// `spreadsheet_id` at version `"1"` (matching [`mount_file`]'s
    /// default).
    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
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
            require_lease: true,
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
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
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
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
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
            lease_token,
            ledger_path,
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
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
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
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::UnprotectRange {
                sheet: Some("Q1".to_string()),
                range: None,
                whole_sheet: true,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ProtectionResult::Changed {
                protected_range_id, ..
            } => assert_eq!(protected_range_id, Some(42)),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    fn protect_verb() -> ProtectionVerb {
        ProtectionVerb::ProtectRange {
            sheet: None,
            range: None,
            whole_sheet: false,
            description: None,
            warning_only: false,
            editors: Vec::new(),
        }
    }

    fn update_verb() -> ProtectionVerb {
        ProtectionVerb::UpdateProtection {
            sheet: None,
            range: None,
            whole_sheet: false,
            description: None,
            warning_only: None,
            add_editors: Vec::new(),
            remove_editors: Vec::new(),
        }
    }

    fn unprotect_verb() -> ProtectionVerb {
        ProtectionVerb::UnprotectRange {
            sheet: None,
            range: None,
            whole_sheet: false,
        }
    }

    #[test]
    fn protection_verb_label_and_log_operation_name_every_variant() {
        assert_eq!(protect_verb().label(), "protect-range");
        assert_eq!(update_verb().label(), "update-protection");
        assert_eq!(unprotect_verb().label(), "unprotect-range");
        assert_eq!(protect_verb().log_operation(), "sheets-protect-range");
        assert_eq!(update_verb().log_operation(), "sheets-update-protection");
        assert_eq!(unprotect_verb().log_operation(), "sheets-unprotect-range");
    }

    #[test]
    fn sheet_range_whole_reads_update_protections_fields() {
        let verb = ProtectionVerb::UpdateProtection {
            sheet: Some("Q1".to_string()),
            range: Some("A1:A5".to_string()),
            whole_sheet: false,
            description: None,
            warning_only: None,
            add_editors: Vec::new(),
            remove_editors: Vec::new(),
        };
        assert_eq!(verb.sheet_range_whole(), (Some("Q1"), Some("A1:A5"), false));
    }

    #[test]
    fn protection_result_log_status_names_every_variant() {
        assert_eq!(
            ProtectionResult::WouldChange {
                summary: String::new()
            }
            .log_status(),
            "would-change"
        );
        assert_eq!(
            ProtectionResult::RefusedNotASpreadsheet {
                mime_type: String::new()
            }
            .log_status(),
            "refused-not-a-spreadsheet"
        );
        assert_eq!(
            ProtectionResult::RefusedShortcut.log_status(),
            "refused-shortcut"
        );
        assert_eq!(
            ProtectionResult::RefusedNoVisibleParents.log_status(),
            "refused-no-visible-parents"
        );
        assert_eq!(
            ProtectionResult::RefusedSheetNotFound {
                title: String::new(),
                available: Vec::new()
            }
            .log_status(),
            "refused-sheet-not-found"
        );
        assert_eq!(
            ProtectionResult::RefusedInvalidRange {
                detail: String::new()
            }
            .log_status(),
            "refused-invalid-range"
        );
        assert_eq!(
            ProtectionResult::RefusedAmbiguousProtection {
                candidates: Vec::new()
            }
            .log_status(),
            "refused-ambiguous-protection"
        );
        assert_eq!(
            ProtectionResult::Failed {
                detail: String::new()
            }
            .log_status(),
            "failed"
        );
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = ProtectionOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: unprotect_verb(),
            result: ProtectionResult::Changed {
                summary: "remove protection".to_string(),
                protected_range_id: Some(1),
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "changed");
    }

    #[test]
    fn compose_target_rejects_whole_sheet_with_range() {
        let verb = ProtectionVerb::ProtectRange {
            sheet: Some("Q1".to_string()),
            range: Some("A1:A5".to_string()),
            whole_sheet: true,
            description: None,
            warning_only: false,
            editors: Vec::new(),
        };
        let err = compose_target(&verb).unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn compose_target_rejects_whole_sheet_without_a_sheet() {
        let verb = ProtectionVerb::ProtectRange {
            sheet: None,
            range: None,
            whole_sheet: true,
            description: None,
            warning_only: false,
            editors: Vec::new(),
        };
        let err = compose_target(&verb).unwrap_err();
        assert!(err.contains("needs --sheet"), "{err}");
    }

    #[test]
    fn validate_verb_rejects_warning_only_protect_range_with_editors() {
        let verb = ProtectionVerb::ProtectRange {
            sheet: Some("Q1".to_string()),
            range: Some("A1:A5".to_string()),
            whole_sheet: false,
            description: None,
            warning_only: true,
            editors: vec!["a@example.com".to_string()],
        };
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn resolve_grid_whole_sheet_without_a_sheet_name_is_refused() {
        let workbook = workbook_with(Vec::new());
        let verb = ProtectionVerb::UnprotectRange {
            sheet: None,
            range: None,
            whole_sheet: true,
        };
        let err = resolve_grid(&workbook, &verb, None).unwrap_err();
        assert!(matches!(err, ProtectionResult::RefusedInvalidRange { .. }));
    }

    #[test]
    fn resolve_grid_reports_a_sheet_not_found() {
        let workbook = workbook_with(Vec::new());
        let err = resolve_grid(&workbook, &protect_verb(), Some("Missing!A1")).unwrap_err();
        assert!(matches!(err, ProtectionResult::RefusedSheetNotFound { .. }));
    }

    #[test]
    fn resolve_grid_reports_an_invalid_range() {
        let workbook = Spreadsheet {
            sheets: vec![Sheet {
                properties: Some(SheetProperties {
                    sheet_id: Some(0),
                    title: "Q1".to_string(),
                    ..Default::default()
                }),
                protected_ranges: Vec::new(),
            }],
            ..Default::default()
        };
        let err = resolve_grid(&workbook, &protect_verb(), Some("Q1!!!!")).unwrap_err();
        assert!(matches!(err, ProtectionResult::RefusedInvalidRange { .. }));
    }

    #[test]
    fn describe_effect_protect_range_reports_strictness_and_editors() {
        let blocking = ProtectionVerb::ProtectRange {
            sheet: None,
            range: None,
            whole_sheet: false,
            description: None,
            warning_only: false,
            editors: vec!["a@example.com".to_string()],
        };
        assert_eq!(
            describe_effect(&blocking),
            "protect (block edits, editors exempted: a@example.com)"
        );

        let warn_only = ProtectionVerb::ProtectRange {
            sheet: None,
            range: None,
            whole_sheet: false,
            description: None,
            warning_only: true,
            editors: Vec::new(),
        };
        assert_eq!(describe_effect(&warn_only), "protect (warn only)");
    }

    #[test]
    fn describe_effect_update_protection_reports_editor_deltas() {
        assert_eq!(describe_effect(&update_verb()), "update protection");

        let both = ProtectionVerb::UpdateProtection {
            sheet: None,
            range: None,
            whole_sheet: false,
            description: None,
            warning_only: None,
            add_editors: vec!["a@example.com".to_string()],
            remove_editors: vec!["b@example.com".to_string()],
        };
        assert_eq!(
            describe_effect(&both),
            "update protection (+a@example.com -b@example.com)"
        );
    }

    fn outcome_with(
        verb: ProtectionVerb,
        file_name: Option<&str>,
        result: ProtectionResult,
    ) -> ProtectionOutcome {
        ProtectionOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: file_name.map(str::to_string),
            resolved_folder_id: None,
            verb,
            result,
        }
    }

    #[test]
    fn describe_lines_renders_would_change() {
        let out = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::WouldChange {
                summary: "protect (block edits)".to_string(),
            },
        );
        assert_eq!(describe(&out), "Would protect (block edits) in 'Budget'");
    }

    #[test]
    fn describe_lines_renders_not_a_spreadsheet_with_no_file_name() {
        let out = outcome_with(
            protect_verb(),
            None,
            ProtectionResult::RefusedNotASpreadsheet {
                mime_type: "text/plain".to_string(),
            },
        );
        let text = describe(&out);
        assert!(text.contains("'sheet-1'"), "{text}");
        assert!(text.contains("protect-range"), "{text}");
        assert!(text.contains("text/plain"), "{text}");
    }

    #[test]
    fn describe_lines_renders_shortcut() {
        let out = outcome_with(
            unprotect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedShortcut,
        );
        let text = describe(&out);
        assert!(text.contains("shortcut"), "{text}");
        assert!(text.contains("unprotect-range"), "{text}");
    }

    #[test]
    fn describe_lines_renders_no_visible_parents() {
        let out = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedNoVisibleParents,
        );
        assert!(describe(&out).contains("sheets-protection"));
    }

    #[test]
    fn describe_lines_renders_sheet_not_found_with_and_without_available_titles() {
        let none = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedSheetNotFound {
                title: "Q2".to_string(),
                available: Vec::new(),
            },
        );
        assert!(describe(&none).contains("Available: none"));

        let some = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedSheetNotFound {
                title: "Q2".to_string(),
                available: vec!["Q1".to_string(), "Q3".to_string()],
            },
        );
        let text = describe(&some);
        assert!(text.contains("'Q1', 'Q3'"), "{text}");
    }

    #[test]
    fn describe_lines_renders_invalid_range_and_protection_not_found() {
        let invalid = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
        );
        assert_eq!(describe(&invalid), "Refused: bad range");

        let not_found = outcome_with(
            update_verb(),
            Some("Budget"),
            ProtectionResult::RefusedProtectionNotFound {
                detail: "no match".to_string(),
            },
        );
        assert_eq!(describe(&not_found), "Refused: no match");
    }

    #[test]
    fn describe_lines_renders_ambiguous_protection() {
        let out = outcome_with(
            update_verb(),
            Some("Budget"),
            ProtectionResult::RefusedAmbiguousProtection {
                candidates: vec![10, 11],
            },
        );
        assert!(describe(&out).contains("ids: 10, 11"));
    }

    #[test]
    fn describe_lines_renders_blocked_with_and_without_a_deciding_rule() {
        let folder_rule = outcome_with(
            update_verb(),
            Some("Budget"),
            ProtectionResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
        );
        let text = describe(&folder_rule);
        assert!(text.contains("update-protection"), "{text}");
        assert!(text.contains("folder folder-1 (depth 2)"), "{text}");

        let default_policy = outcome_with(
            unprotect_verb(),
            Some("Budget"),
            ProtectionResult::Blocked { decided_by: None },
        );
        assert!(describe(&default_policy).contains("default policy"));
    }

    #[test]
    fn describe_lines_renders_changed_with_and_without_an_id() {
        let with_id = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::Changed {
                summary: "protect (block edits)".to_string(),
                protected_range_id: Some(7),
            },
        );
        assert!(describe(&with_id).contains("(id 7)"));

        let without_id = outcome_with(
            unprotect_verb(),
            Some("Budget"),
            ProtectionResult::Changed {
                summary: "remove protection".to_string(),
                protected_range_id: None,
            },
        );
        assert!(!describe(&without_id).contains("(id"));
    }

    #[test]
    fn describe_lines_renders_failed() {
        let out = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::Failed {
                detail: "boom".to_string(),
            },
        );
        assert_eq!(describe(&out), "Failed: boom");
    }

    #[test]
    fn describe_lines_renders_every_lease_refusal_with_the_lease_acquire_hint() {
        let no_lease = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedNoLease,
        );
        let text = describe(&no_lease);
        assert!(text.contains("requires a Drive write lease"), "{text}");
        assert!(text.contains("drive lease acquire sheet-1"), "{text}");

        let expired = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedLeaseExpired,
        );
        let text = describe(&expired);
        assert!(text.contains("expired, released, or unknown"), "{text}");
        assert!(text.contains("drive lease acquire sheet-1"), "{text}");

        let wrong_file = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedLeaseWrongFile,
        );
        let text = describe(&wrong_file);
        assert!(text.contains("acquired for a different file"), "{text}");
        assert!(text.contains("drive lease acquire sheet-1"), "{text}");

        let stale = outcome_with(
            protect_verb(),
            Some("Budget"),
            ProtectionResult::RefusedLeaseStale,
        );
        let text = describe(&stale);
        assert!(
            text.contains("changed since the lease was acquired"),
            "{text}"
        );
        assert!(text.contains("drive lease acquire sheet-1"), "{text}");
    }

    #[tokio::test]
    async fn a_metadata_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
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
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_shortcut_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["folder-1"],
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
                description: None,
                warning_only: false,
                editors: Vec::new(),
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::RefusedShortcut));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn a_non_spreadsheet_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.document",
            &["folder-1"],
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
                description: None,
                warning_only: false,
                editors: Vec::new(),
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::RefusedNotASpreadsheet { .. }
        ));
    }

    #[tokio::test]
    async fn a_target_with_no_visible_parents_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", crate::drive::types::GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
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
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::RefusedNoVisibleParents
        ));
    }

    #[tokio::test]
    async fn a_gate_ancestor_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/folder-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
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
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_workbook_fetch_failure_after_a_granted_gate_surfaces_as_failed() {
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
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
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
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Failed { .. }));
    }

    #[tokio::test]
    async fn protect_range_rejects_whole_sheet_and_range_together() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::ProtectRange {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: true,
                description: None,
                warning_only: false,
                editors: Vec::new(),
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ProtectionResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("mutually exclusive"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn protect_range_rejects_warning_only_with_editors() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::ProtectRange {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
                description: None,
                warning_only: true,
                editors: vec!["a@example.com".to_string()],
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ProtectionResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("mutually exclusive"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn protect_range_refuses_an_unknown_sheet() {
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
            verb: ProtectionVerb::ProtectRange {
                sheet: Some("Missing".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
                description: None,
                warning_only: false,
                editors: Vec::new(),
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::RefusedSheetNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn a_dry_run_reports_would_change_without_calling_batch_update() {
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
        // Deliberately no batchUpdate mock: proves a dry run never calls it.
        let rules = vec![allow_rule("folder-1")];
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
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::WouldChange { .. }
        ));
    }

    #[tokio::test]
    async fn protect_range_with_editors_sends_an_editor_list() {
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
                    "replies": [{"addProtectedRange": {"protectedRange": {"protectedRangeId": 5}}}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::ProtectRange {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
                description: None,
                warning_only: false,
                editors: vec!["a@example.com".to_string()],
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Changed { .. }));
    }

    #[tokio::test]
    async fn update_protection_changes_description_and_editors() {
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
            "protectedRangeId": 20,
            "range": {
                "sheetId": 0,
                "startRowIndex": 0,
                "endRowIndex": 5,
                "startColumnIndex": 0,
                "endColumnIndex": 1,
            },
            "editors": {"users": ["a@example.com"]},
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
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::UpdateProtection {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
                description: Some("new note".to_string()),
                warning_only: None,
                add_editors: vec!["b@example.com".to_string()],
                remove_editors: vec!["a@example.com".to_string()],
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ProtectionResult::Changed {
                protected_range_id, ..
            } => assert_eq!(protected_range_id, Some(20)),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_protection_refuses_a_result_that_is_warning_only_with_editors() {
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
            "protectedRangeId": 30,
            "range": {
                "sheetId": 0,
                "startRowIndex": 0,
                "endRowIndex": 5,
                "startColumnIndex": 0,
                "endColumnIndex": 1,
            },
            "warningOnly": true,
        }]))
        .mount(&server)
        .await;
        // Deliberately no batchUpdate mock: proves the invalid combination
        // is refused before any request is sent.
        let rules = vec![allow_rule("folder-1")];
        let opts = ProtectionOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ProtectionVerb::UpdateProtection {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
                description: None,
                warning_only: None,
                add_editors: vec!["a@example.com".to_string()],
                remove_editors: Vec::new(),
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ProtectionResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("warning-only"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_batch_update_failure_surfaces_as_failed() {
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
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
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
            lease_token,
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Failed { .. }));
    }

    // ── the Drive write lease (ADR-0080 §9) ────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
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
        // No batchUpdate mock mounted — a refusal must make zero mutating
        // calls.
        let rules = vec![allow_rule("folder-1")];

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
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::RefusedNoLease));
        assert_eq!(outcome.result.log_status(), "refused-no-lease");
    }

    #[tokio::test]
    async fn reports_a_lock_acquisition_failure_as_failed() {
        // A pre-existing lock file simulates another `drive lease`
        // operation genuinely in progress — reported as an operational
        // failure, not folded into `RefusedLeaseExpired`.
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
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let mut lock_path = ledger_path.clone().into_os_string();
        lock_path.push(".lock");
        std::fs::write(std::path::PathBuf::from(lock_path), b"").unwrap();

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
            lease_token,
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_failed_pre_lease_refetch_is_reported_as_failed_with_no_batch_update_call() {
        // The gate's own resolve step succeeds off the first `files.get`,
        // but the fresh re-fetch feeding the staleness check (ADR-0080 §6)
        // fails — the change must report `Failed` and never reach
        // `batchUpdate`.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([])).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");

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
            lease_token,
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Failed { .. }));
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
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
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        // Never seeded — the ledger exists nowhere near this token.

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
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::RefusedLeaseExpired
        ));
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
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
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        // Seeded for a *different* spreadsheet id.
        let token = seed_lease(&ledger_path, "some-other-sheet", "1");

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
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::RefusedLeaseWrongFile
        ));
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // `mount_file` always returns version "1"; the lease below was
        // acquired against version "0" — a foreign edit landed since.
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
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");

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
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ProtectionResult::RefusedLeaseStale
        ));
    }

    #[tokio::test]
    async fn require_lease_false_skips_the_lease_check_entirely() {
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
                    "replies": [{"addProtectedRange": {"protectedRange": {"protectedRangeId": 1}}}]
                })),
            )
            .mount(&server)
            .await;
        let rule = FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsProtection).collect(),
            deny: HashSet::default(),
            require_lease: false,
        };

        // No lease token presented at all, and no ledger exists.
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
            lease_token: None,
            ledger_path: std::path::PathBuf::from("/nonexistent/lease-ledger.jsonl"),
        };
        let outcome = protection(&drive, &sheets, &opts, &[rule]).await;
        assert!(matches!(outcome.result, ProtectionResult::Changed { .. }));
    }

    // ── the write's own audit trail (ADR-0080 §11) ─────────────────────

    #[tokio::test]
    async fn a_leased_protection_change_concludes_its_audit_pair_with_allowed() {
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
        let dir = tempfile::tempdir().unwrap();
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
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
            lease_token,
            ledger_path,
        };

        let outcome = protection(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ProtectionResult::Changed { .. }));

        let records = audit.records();
        assert_eq!(audit.verdicts(), ["pending", "allowed"], "{records:?}");
        // The verb, not the engine — the same `["drive", <log_operation>]`
        // this write's `drivemutation` record carries, so an auditor can
        // see which verb ran without joining back to `log.jsonl`.
        assert_eq!(records[0].command, ["drive", "sheets-protect-range"]);
    }
}
