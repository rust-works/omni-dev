//! Banded ranges — alternating row/column colors — via
//! `spreadsheets.batchUpdate` (issue #1832,
//! [ADR-0082](../../../docs/adrs/adr-0082-banded-ranges.md)).
//!
//! Gated by [`DriveOperation::SheetsStructure`] — every mutating verb here
//! reaches the same operation as `unmerge-cells`/`clear-data-validation`
//! (ADR-0078's original reasoning): a banding is presentation applied to a
//! range, and removing one destroys no data.
//!
//! A banded range, like a filter view, carries a server-assigned
//! `bandedRangeId` — the stable handle `list-bandings` discovers and
//! `update-banding`/`delete-banding` take directly via
//! `--banded-range-id`, mirroring `filter.rs`'s filter-view addressing
//! rather than `protection.rs`'s range-equality match (there is no
//! ambiguous-match case to handle: ids are unique by construction).
//!
//! **Two documented cuts, not silent gaps**, matching `filter.rs`'s own
//! framing:
//!
//! - **Only the modern `*ColorStyle` fields are modelled** — never the
//!   deprecated plain `Color` fields, never the `themeColor` arm within a
//!   `ColorStyle`. Same precedent as `format-cells` (`format.rs`'s
//!   `parse_hex_color`, reused here unchanged).
//! - **At most one of `rowProperties`/`columnProperties` per call,
//!   selected by `--axis`.** The API allows both simultaneously on one
//!   `BandedRange` (a checkerboard effect), but that would double every
//!   color flag for a combination nobody asked for.
//!
//! Shape mirrors `filter.rs`: compose a target, resolve it against a
//! freshly-fetched workbook, gate, dry-run, mutate, log.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::check::{
    conclude_native_leased_write, gate_optional_leased_write, FromLeaseRefusal, LeaseGateRefusal,
    LeasedWrite,
};
use crate::drive::sheets::a1;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::format::parse_hex_color;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AddBandingRequest, BandedRange, BandingProperties, BatchUpdateRequestItem, BatchUpdateResponse,
    ColorStyle, DeleteBandingRequest, GridRange, Spreadsheet, UpdateBandingRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which axis a banding alternates along — selects
/// [`BandedRange::row_properties`] vs [`BandedRange::column_properties`].
///
/// Deliberately carries no `clap` dependency — the CLI-layer mirror
/// `crate::cli::drive::sheets::banding::BandingAxisArg` is a
/// `clap::ValueEnum`, converted into this at the CLI boundary, the same
/// split `conditional_format.rs`'s `GradientMidTypeArg`/`GradientPointType`
/// uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandingAxis {
    /// Alternate by row (the common case — the Sheets UI's default).
    Rows,
    /// Alternate by column.
    Columns,
}

impl BandingAxis {
    const fn label(self) -> &'static str {
        match self {
            Self::Rows => "row",
            Self::Columns => "column",
        }
    }

    const fn field_name(self) -> &'static str {
        match self {
            Self::Rows => "rowProperties",
            Self::Columns => "columnProperties",
        }
    }
}

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BandingVerb {
    /// Add a new banded range.
    AddBanding {
        /// The sheet to band.
        sheet: String,
        /// The banded A1 range, optionally carrying its own `Sheet!` prefix.
        range: String,
        /// Which axis to alternate along.
        axis: BandingAxis,
        /// The header row/column's color, `#RRGGBB`, if distinct from the
        /// alternating bands.
        header_color: Option<String>,
        /// The first band's color, `#RRGGBB`.
        first_band_color: String,
        /// The second (alternating) band's color, `#RRGGBB`.
        second_band_color: String,
        /// The footer row/column's color, `#RRGGBB`, if distinct from the
        /// alternating bands.
        footer_color: Option<String>,
    },
    /// Change an existing banded range's range and/or colors.
    UpdateBanding {
        /// Which banded range to change, discovered via `list-bandings`.
        banded_range_id: i64,
        /// A sheet title, supplying a prefix for a bare `range`, when
        /// changing the banded range.
        sheet: Option<String>,
        /// The new banded A1 range, when changing it.
        range: Option<String>,
        /// Which axis's properties to update.
        axis: BandingAxis,
        /// The new header color, `#RRGGBB`, when changing it.
        header_color: Option<String>,
        /// The new first-band color, `#RRGGBB`, when changing it.
        first_band_color: Option<String>,
        /// The new second-band color, `#RRGGBB`, when changing it.
        second_band_color: Option<String>,
        /// The new footer color, `#RRGGBB`, when changing it.
        footer_color: Option<String>,
    },
    /// Remove a banded range.
    DeleteBanding {
        /// Which banded range to remove, discovered via `list-bandings`.
        banded_range_id: i64,
    },
}

impl BandingVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::AddBanding { .. } => "sheets-add-banding",
            Self::UpdateBanding { .. } => "sheets-update-banding",
            Self::DeleteBanding { .. } => "sheets-delete-banding",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::AddBanding { .. } => "add-banding",
            Self::UpdateBanding { .. } => "update-banding",
            Self::DeleteBanding { .. } => "delete-banding",
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct BandingOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: BandingVerb,
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
pub enum BandingResult {
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
    /// The verb's own arguments were invalid — a bad `--sheet`/`--range`
    /// pair, a malformed color, or (for `update-banding`) nothing to
    /// change.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `update-banding`/`delete-banding` named a `--banded-range-id` that
    /// does not exist in this workbook.
    RefusedBandedRangeNotFound {
        /// The id that was not found.
        banded_range_id: i64,
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
        /// The banded range's stable id — server-assigned for
        /// `add-banding`, otherwise the one resolved against.
        banded_range_id: Option<i64>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for BandingResult {
    fn from_no_lease() -> Self {
        Self::RefusedNoLease
    }
    fn from_lease_expired() -> Self {
        Self::RefusedLeaseExpired
    }
    fn from_lease_wrong_file() -> Self {
        Self::RefusedLeaseWrongFile
    }
    fn from_lease_stale() -> Self {
        Self::RefusedLeaseStale
    }
    fn from_lease_failed(detail: String) -> Self {
        Self::Failed { detail }
    }
}

impl BandingResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedBandedRangeNotFound { .. } => "refused-banded-range-not-found",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => LeaseGateRefusal::NoLease.log_status(),
            Self::RefusedLeaseExpired => LeaseGateRefusal::Expired.log_status(),
            Self::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.log_status(),
            Self::RefusedLeaseStale => LeaseGateRefusal::Stale.log_status(),
            Self::Changed { .. } => "changed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BandingOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// The sheet the banded range is (or would be) on, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_id: Option<i64>,
    /// Which mutation was attempted. Not serialised.
    #[serde(skip)]
    pub verb: BandingVerb,
    /// What happened.
    pub result: BandingResult,
}

impl JsonlSerialize for BandingOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one banding mutation, logging every attempt that isn't a dry run.
pub async fn banding(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &BandingOptions,
    rules: &[FolderPermissionRule],
) -> BandingOutcome {
    let started = Instant::now();
    let outcome = banding_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn banding_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &BandingOptions,
    rules: &[FolderPermissionRule],
) -> BandingOutcome {
    let bare = |result| BandingOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        sheet_id: None,
        verb: opts.verb.clone(),
        result,
    };

    if let Err(detail) = validate_verb(&opts.verb) {
        return bare(BandingResult::RefusedInvalidRange { detail });
    }
    let composed_range = match compose_target(&opts.verb) {
        Ok(composed) => composed,
        Err(detail) => return bare(BandingResult::RefusedInvalidRange { detail }),
    };

    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsStructure,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(BandingResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => BandingResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    BandingResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => BandingResult::RefusedNoVisibleParents,
            };
            return BandingOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return BandingOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                verb: opts.verb.clone(),
                result: BandingResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let pre_gated = |result| BandingOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: None,
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return pre_gated(BandingResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet_with_banding(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return pre_gated(BandingResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let resolved_target =
        match resolve_sheet_target(&workbook, &opts.verb, composed_range.as_deref()) {
            Ok(resolved) => resolved,
            Err(result) => return pre_gated(result),
        };

    let existing = match &opts.verb {
        BandingVerb::UpdateBanding {
            banded_range_id, ..
        }
        | BandingVerb::DeleteBanding { banded_range_id } => {
            match find_existing_banded_range(&workbook, *banded_range_id) {
                Ok(existing) => Some(existing),
                Err(result) => return pre_gated(result),
            }
        }
        BandingVerb::AddBanding { .. } => None,
    };

    let sheet_id = resolved_target.map(|grid| grid.sheet_id).or_else(|| {
        existing
            .and_then(|existing| existing.range.as_ref())
            .map(|range| range.sheet_id)
    });

    let gated = |result| BandingOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id,
        verb: opts.verb.clone(),
        result,
    };

    let summary = describe_effect(&opts.verb);

    if opts.dry_run {
        return gated(BandingResult::WouldChange { summary });
    }

    // Built before the gate, not after — see `format.rs::format_inner`'s
    // doc comment for the full `#1688` ordering reasoning, shared verbatim
    // by every leased engine: the gate must be the last fallible step
    // before the mutating call, so `parse_hex_color`'s validation happens
    // here, inside `build_request`, rather than earlier.
    let (request, existing_id) = match build_request(&opts.verb, resolved_target, existing) {
        Ok(built) => built,
        Err(detail) => return gated(BandingResult::RefusedInvalidRange { detail }),
    };

    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets banding",
        operation: opts.verb.log_operation(),
        ledger_path: &opts.ledger_path,
        file_id: &opts.spreadsheet_id,
    };
    let lease_grant = match gate_optional_leased_write(
        leased,
        &files_api,
        requires_lease,
        opts.lease_token.as_deref(),
    )
    .await
    {
        Ok(grant) => grant,
        Err(err) => return gated(err.into_result()),
    };

    let result = match conclude_native_leased_write(
        leased,
        &lease_grant,
        &files_api,
        api.batch_update(&opts.spreadsheet_id, vec![request]).await,
        |err| format!("{err:#}"),
    )
    .await
    {
        Ok(response) => {
            let banded_range_id = added_banded_range_id(&response).or(existing_id);
            BandingResult::Changed {
                summary,
                banded_range_id,
            }
        }
        Err(err) => BandingResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Composes the `--sheet`/`--range` pair a verb carries into one string,
/// exactly like `filter.rs`/`protection.rs`/`format.rs`. `DeleteBanding`
/// never calls this — it carries no range flags at all.
fn compose_target(verb: &BandingVerb) -> Result<Option<String>, String> {
    match verb {
        BandingVerb::AddBanding { sheet, range, .. } => a1::compose(Some(sheet), Some(range))
            .map(Some)
            .map_err(|err| err.to_string()),
        BandingVerb::UpdateBanding { sheet, range, .. } => {
            match (sheet.as_deref(), range.as_deref()) {
                (None, None) => Ok(None),
                (sheet, range) => a1::compose(sheet, range)
                    .map(Some)
                    .map_err(|err| err.to_string()),
            }
        }
        BandingVerb::DeleteBanding { .. } => Ok(None),
    }
}

/// Rejects a verb whose own arguments are internally inconsistent, cheaply,
/// before ever fetching the workbook.
fn validate_verb(verb: &BandingVerb) -> Result<(), String> {
    if let BandingVerb::UpdateBanding {
        sheet,
        range,
        header_color,
        first_band_color,
        second_band_color,
        footer_color,
        ..
    } = verb
    {
        let nothing_to_change = sheet.is_none()
            && range.is_none()
            && header_color.is_none()
            && first_band_color.is_none()
            && second_band_color.is_none()
            && footer_color.is_none();
        if nothing_to_change {
            return Err(
                "nothing to change: pass --sheet/--range, --header-color, --first-band-color, \
                 --second-band-color, or --footer-color"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// Resolves the sheet and [`GridRange`] a verb targets, once, so callers
/// never need to re-parse the same `--sheet`/`--range` composition a second
/// time. `AddBanding` always resolves one; `UpdateBanding` may target no
/// sheet at all (leaving the existing banded range's range untouched),
/// hence the `Option`; `DeleteBanding` never does.
fn resolve_sheet_target(
    workbook: &Spreadsheet,
    verb: &BandingVerb,
    composed: Option<&str>,
) -> Result<Option<GridRange>, BandingResult> {
    match verb {
        BandingVerb::AddBanding { .. } => {
            let composed = composed.unwrap_or_default();
            let (_, grid) = grid_range::resolve_grid_range(
                workbook,
                composed,
                |detail| BandingResult::RefusedInvalidRange { detail },
                |title, available| BandingResult::RefusedSheetNotFound { title, available },
            )?;
            Ok(Some(grid))
        }
        BandingVerb::UpdateBanding { .. } => match composed {
            Some(composed) => {
                let (_, grid) = grid_range::resolve_grid_range(
                    workbook,
                    composed,
                    |detail| BandingResult::RefusedInvalidRange { detail },
                    |title, available| BandingResult::RefusedSheetNotFound { title, available },
                )?;
                Ok(Some(grid))
            }
            None => Ok(None),
        },
        BandingVerb::DeleteBanding { .. } => Ok(None),
    }
}

/// Finds the one banded range whose id exactly matches `banded_range_id`.
/// Banded ranges are directly id-addressed (the id comes from
/// `list-bandings`), so unlike `protection.rs::find_existing_protection`
/// there is no ambiguous-match case — ids are unique by construction.
fn find_existing_banded_range(
    workbook: &Spreadsheet,
    banded_range_id: i64,
) -> Result<&BandedRange, BandingResult> {
    workbook
        .sheets
        .iter()
        .flat_map(|sheet| sheet.banded_ranges.iter())
        .find(|banded| banded.banded_range_id == Some(banded_range_id))
        .ok_or(BandingResult::RefusedBandedRangeNotFound { banded_range_id })
}

/// Parses a `#RRGGBB` flag into a [`ColorStyle`], via `format.rs`'s shared
/// [`parse_hex_color`].
fn parse_color_style(hex: &str) -> Result<ColorStyle, String> {
    Ok(ColorStyle {
        rgb_color: parse_hex_color(hex)?,
    })
}

/// Builds the request for `verb`. Placed after the `--dry-run` branch in
/// `banding_inner` deliberately — see that function's comment just before
/// the call site.
fn build_request(
    verb: &BandingVerb,
    resolved: Option<GridRange>,
    existing: Option<&BandedRange>,
) -> Result<(BatchUpdateRequestItem, Option<i64>), String> {
    match verb {
        BandingVerb::AddBanding {
            axis,
            header_color,
            first_band_color,
            second_band_color,
            footer_color,
            ..
        } => {
            let Some(range) = resolved else {
                unreachable!("resolved is resolved for AddBanding above") // omni-dev: coverage ignore-line reason="resolve_sheet_target always resolves AddBanding to a range or has already returned its refusal; this else-arm exists only to unwrap the shared Option"
            };
            let properties = BandingProperties {
                header_color_style: header_color.as_deref().map(parse_color_style).transpose()?,
                first_band_color_style: Some(parse_color_style(first_band_color)?),
                second_band_color_style: Some(parse_color_style(second_band_color)?),
                footer_color_style: footer_color.as_deref().map(parse_color_style).transpose()?,
            };
            let mut banded_range = BandedRange {
                banded_range_id: None,
                range: Some(range),
                row_properties: None,
                column_properties: None,
            };
            match axis {
                BandingAxis::Rows => banded_range.row_properties = Some(properties),
                BandingAxis::Columns => banded_range.column_properties = Some(properties),
            }
            Ok((
                BatchUpdateRequestItem::AddBanding(AddBandingRequest { banded_range }),
                None,
            ))
        }
        BandingVerb::UpdateBanding {
            banded_range_id,
            axis,
            header_color,
            first_band_color,
            second_band_color,
            footer_color,
            ..
        } => {
            let Some(existing) = existing else {
                unreachable!("existing is resolved for UpdateBanding above") // omni-dev: coverage ignore-line reason="find_existing_banded_range returns Some for UpdateBanding or has already returned RefusedBandedRangeNotFound; this else-arm exists only to unwrap the shared Option"
            };
            let update = build_update(
                existing,
                *banded_range_id,
                *axis,
                resolved,
                header_color.as_deref(),
                first_band_color.as_deref(),
                second_band_color.as_deref(),
                footer_color.as_deref(),
            )?;
            Ok((
                BatchUpdateRequestItem::UpdateBanding(update),
                Some(*banded_range_id),
            ))
        }
        BandingVerb::DeleteBanding { banded_range_id } => Ok((
            BatchUpdateRequestItem::DeleteBanding(DeleteBandingRequest {
                banded_range_id: *banded_range_id,
            }),
            Some(*banded_range_id),
        )),
    }
}

/// Builds the `updateBanding` request, merging any changed color onto the
/// selected axis's *existing* properties — the full resulting state, since
/// Sheets' `fields` mask replaces a named field (`rowProperties`/
/// `columnProperties`) wholesale, never merging per-sub-field, matching
/// `filter.rs::build_update`'s `sortSpecs`/`criteria` reasoning. An unset
/// color flag leaves that color untouched; the axis field is included in
/// the mask only when at least one color actually changed.
#[allow(clippy::too_many_arguments)]
fn build_update(
    existing: &BandedRange,
    banded_range_id: i64,
    axis: BandingAxis,
    range: Option<GridRange>,
    header_color: Option<&str>,
    first_band_color: Option<&str>,
    second_band_color: Option<&str>,
    footer_color: Option<&str>,
) -> Result<UpdateBandingRequest, String> {
    let mut fields = Vec::new();
    let mut banded_range = BandedRange {
        banded_range_id: Some(banded_range_id),
        range: None,
        row_properties: None,
        column_properties: None,
    };
    if let Some(range) = range {
        banded_range.range = Some(range);
        fields.push("range");
    }

    let properties_changed = header_color.is_some()
        || first_band_color.is_some()
        || second_band_color.is_some()
        || footer_color.is_some();
    let mut properties = match axis {
        BandingAxis::Rows => existing.row_properties.clone().unwrap_or_default(),
        BandingAxis::Columns => existing.column_properties.clone().unwrap_or_default(),
    };
    if let Some(hex) = header_color {
        properties.header_color_style = Some(parse_color_style(hex)?);
    }
    if let Some(hex) = first_band_color {
        properties.first_band_color_style = Some(parse_color_style(hex)?);
    }
    if let Some(hex) = second_band_color {
        properties.second_band_color_style = Some(parse_color_style(hex)?);
    }
    if let Some(hex) = footer_color {
        properties.footer_color_style = Some(parse_color_style(hex)?);
    }
    if properties_changed {
        match axis {
            BandingAxis::Rows => banded_range.row_properties = Some(properties),
            BandingAxis::Columns => banded_range.column_properties = Some(properties),
        }
        fields.push(axis.field_name());
    }

    Ok(UpdateBandingRequest {
        banded_range,
        fields: fields.join(","),
    })
}

fn added_banded_range_id(response: &BatchUpdateResponse) -> Option<i64> {
    response
        .replies
        .iter()
        .find_map(|reply| reply.add_banding.as_ref())
        .and_then(|added| added.banded_range.as_ref())
        .and_then(|banded| banded.banded_range_id)
}

fn describe_effect(verb: &BandingVerb) -> String {
    match verb {
        BandingVerb::AddBanding { axis, .. } => format!("add {} banding", axis.label()),
        BandingVerb::UpdateBanding {
            banded_range_id,
            axis,
            ..
        } => format!("update {} banding id {banded_range_id}", axis.label()),
        BandingVerb::DeleteBanding { banded_range_id } => {
            format!("delete banding id {banded_range_id}")
        }
    }
}

fn record_attempt(outcome: &BandingOutcome, opts: &BandingOptions, duration: Duration) {
    let error = match &outcome.result {
        BandingResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        BandingResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let banded_range_id = match &outcome.result {
        BandingResult::Changed {
            banded_range_id, ..
        } => *banded_range_id,
        _ => None,
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
        sheet_id: outcome.sheet_id,
        banded_range_id,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &BandingOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &BandingOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        BandingResult::WouldChange { summary } => vec![format!("Would {summary} in {book}")],
        BandingResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        BandingResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        BandingResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        BandingResult::RefusedSheetNotFound { title, available } => {
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
        BandingResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        BandingResult::RefusedBandedRangeNotFound { banded_range_id } => vec![format!(
            "Refused: {book} has no banded range with id {banded_range_id}; run \
             `drive sheets list-bandings` to see what exists"
        )],
        BandingResult::Blocked { decided_by } => vec![match decided_by {
            Some(rule) => format!(
                "Blocked: {} on {book} refused by rule on {} {}{}",
                verb.label(),
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: {} on {book} refused by default policy (no matching rule for \
                 sheets-structure)",
                verb.label()
            ),
        }],
        BandingResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        BandingResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        BandingResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        BandingResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        BandingResult::Changed {
            summary,
            banded_range_id,
        } => {
            let id = banded_range_id.map_or_else(String::new, |id| format!(" (id {id})"));
            vec![format!("Applied: {summary}{id} in {book}")]
        }
        BandingResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::Sheet;
    use crate::drive::test_support::seed_lease;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    // ── pure helpers ─────────────────────────────────────────────────────

    #[test]
    fn log_operation_and_label_are_distinct_for_every_verb() {
        let verbs = [
            BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            BandingVerb::UpdateBanding {
                banded_range_id: 1,
                sheet: None,
                range: None,
                axis: BandingAxis::Rows,
                header_color: Some("#000000".to_string()),
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
            },
            BandingVerb::DeleteBanding { banded_range_id: 1 },
        ];
        let mut operations: Vec<&str> = verbs.iter().map(BandingVerb::log_operation).collect();
        let mut labels: Vec<&str> = verbs.iter().map(BandingVerb::label).collect();
        operations.sort_unstable();
        operations.dedup();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(operations.len(), verbs.len());
        assert_eq!(labels.len(), verbs.len());
    }

    #[test]
    fn validate_verb_rejects_update_banding_with_nothing_to_change() {
        let verb = BandingVerb::UpdateBanding {
            banded_range_id: 1,
            sheet: None,
            range: None,
            axis: BandingAxis::Rows,
            header_color: None,
            first_band_color: None,
            second_band_color: None,
            footer_color: None,
        };
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("nothing to change"), "{err}");
    }

    #[test]
    fn validate_verb_accepts_update_banding_with_only_a_color_change() {
        let verb = BandingVerb::UpdateBanding {
            banded_range_id: 1,
            sheet: None,
            range: None,
            axis: BandingAxis::Rows,
            header_color: Some("#000000".to_string()),
            first_band_color: None,
            second_band_color: None,
            footer_color: None,
        };
        validate_verb(&verb).unwrap();
    }

    #[test]
    fn banding_axis_label_and_field_name_cover_both_axes() {
        assert_eq!(BandingAxis::Rows.label(), "row");
        assert_eq!(BandingAxis::Columns.label(), "column");
        assert_eq!(BandingAxis::Rows.field_name(), "rowProperties");
        assert_eq!(BandingAxis::Columns.field_name(), "columnProperties");
    }

    fn sheet_with_banding(sheet_id: i64, banded_range_id: i64) -> Sheet {
        Sheet {
            banded_ranges: vec![BandedRange {
                banded_range_id: Some(banded_range_id),
                range: Some(GridRange {
                    sheet_id,
                    start_row_index: Some(0),
                    end_row_index: Some(10),
                    start_column_index: Some(0),
                    end_column_index: Some(4),
                }),
                row_properties: Some(BandingProperties {
                    header_color_style: None,
                    first_band_color_style: Some(ColorStyle {
                        rgb_color: crate::drive::sheets::types::Color {
                            red: 1.0,
                            green: 1.0,
                            blue: 1.0,
                        },
                    }),
                    second_band_color_style: Some(ColorStyle {
                        rgb_color: crate::drive::sheets::types::Color {
                            red: 0.0,
                            green: 0.0,
                            blue: 0.0,
                        },
                    }),
                    footer_color_style: None,
                }),
                column_properties: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn find_existing_banded_range_returns_not_found_for_unknown_id() {
        let workbook = Spreadsheet {
            sheets: vec![sheet_with_banding(0, 7)],
            ..Default::default()
        };
        let err = find_existing_banded_range(&workbook, 99).unwrap_err();
        assert_eq!(
            err,
            BandingResult::RefusedBandedRangeNotFound {
                banded_range_id: 99
            }
        );
    }

    #[test]
    fn find_existing_banded_range_finds_the_matching_id() {
        let workbook = Spreadsheet {
            sheets: vec![sheet_with_banding(0, 7)],
            ..Default::default()
        };
        let found = find_existing_banded_range(&workbook, 7).unwrap();
        assert_eq!(found.banded_range_id, Some(7));
    }

    #[test]
    fn build_request_add_banding_sets_row_properties_for_the_rows_axis() {
        let range = GridRange {
            sheet_id: 0,
            start_row_index: Some(0),
            end_row_index: Some(10),
            start_column_index: Some(0),
            end_column_index: Some(4),
        };
        let verb = BandingVerb::AddBanding {
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            axis: BandingAxis::Rows,
            header_color: Some("#111111".to_string()),
            first_band_color: "#FFFFFF".to_string(),
            second_band_color: "#EEEEEE".to_string(),
            footer_color: None,
        };
        let (request, existing_id) = build_request(&verb, Some(range), None).unwrap();
        assert_eq!(existing_id, None);
        let BatchUpdateRequestItem::AddBanding(add) = request else {
            panic!("expected AddBanding"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns AddBanding for a BandingVerb::AddBanding verb"
        };
        assert_eq!(add.banded_range.banded_range_id, None);
        assert_eq!(add.banded_range.range, Some(range));
        assert!(add.banded_range.row_properties.is_some());
        assert!(add.banded_range.column_properties.is_none());
        let properties = add.banded_range.row_properties.unwrap();
        assert!(properties.header_color_style.is_some());
        assert!(properties.footer_color_style.is_none());
    }

    #[test]
    fn build_request_add_banding_sets_column_properties_for_the_columns_axis() {
        let range = GridRange {
            sheet_id: 0,
            ..Default::default()
        };
        let verb = BandingVerb::AddBanding {
            sheet: "Q1".to_string(),
            range: "A:D".to_string(),
            axis: BandingAxis::Columns,
            header_color: None,
            first_band_color: "#FFFFFF".to_string(),
            second_band_color: "#EEEEEE".to_string(),
            footer_color: None,
        };
        let (request, _) = build_request(&verb, Some(range), None).unwrap();
        let BatchUpdateRequestItem::AddBanding(add) = request else {
            panic!("expected AddBanding"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns AddBanding for a BandingVerb::AddBanding verb"
        };
        assert!(add.banded_range.row_properties.is_none());
        assert!(add.banded_range.column_properties.is_some());
    }

    #[test]
    fn build_request_add_banding_rejects_a_malformed_color() {
        let range = GridRange {
            sheet_id: 0,
            ..Default::default()
        };
        let verb = BandingVerb::AddBanding {
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            axis: BandingAxis::Rows,
            header_color: None,
            first_band_color: "not-a-color".to_string(),
            second_band_color: "#EEEEEE".to_string(),
            footer_color: None,
        };
        let err = build_request(&verb, Some(range), None).unwrap_err();
        assert!(err.contains("not a color"), "{err}");
    }

    #[test]
    fn build_request_update_banding_merges_onto_the_existing_axis_without_touching_unset_colors() {
        let existing = sheet_with_banding(0, 7).banded_ranges.remove(0);
        let verb = BandingVerb::UpdateBanding {
            banded_range_id: 7,
            sheet: None,
            range: None,
            axis: BandingAxis::Rows,
            header_color: Some("#123456".to_string()),
            first_band_color: None,
            second_band_color: None,
            footer_color: None,
        };
        let (request, existing_id) = build_request(&verb, None, Some(&existing)).unwrap();
        assert_eq!(existing_id, Some(7));
        let BatchUpdateRequestItem::UpdateBanding(update) = request else {
            panic!("expected UpdateBanding"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns UpdateBanding for a BandingVerb::UpdateBanding verb"
        };
        assert_eq!(update.fields, "rowProperties");
        assert!(update.banded_range.range.is_none());
        let properties = update.banded_range.row_properties.unwrap();
        assert!(properties.header_color_style.is_some());
        // The existing first/second band colors survive untouched — only
        // the header color was named.
        assert_eq!(
            properties.first_band_color_style,
            existing.row_properties.unwrap().first_band_color_style
        );
    }

    #[test]
    fn build_request_update_banding_changes_every_color_field() {
        let existing = sheet_with_banding(0, 7).banded_ranges.remove(0);
        let verb = BandingVerb::UpdateBanding {
            banded_range_id: 7,
            sheet: None,
            range: None,
            axis: BandingAxis::Rows,
            header_color: Some("#123456".to_string()),
            first_band_color: Some("#111111".to_string()),
            second_band_color: Some("#222222".to_string()),
            footer_color: Some("#333333".to_string()),
        };
        let (request, _) = build_request(&verb, None, Some(&existing)).unwrap();
        let BatchUpdateRequestItem::UpdateBanding(update) = request else {
            panic!("expected UpdateBanding"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns UpdateBanding for a BandingVerb::UpdateBanding verb"
        };
        assert_eq!(update.fields, "rowProperties");
        let properties = update.banded_range.row_properties.unwrap();
        assert!(properties.header_color_style.is_some());
        assert!(properties.first_band_color_style.is_some());
        assert!(properties.second_band_color_style.is_some());
        assert!(properties.footer_color_style.is_some());
    }

    #[test]
    fn build_request_update_banding_sets_column_properties_for_the_columns_axis() {
        let existing = sheet_with_banding(0, 7).banded_ranges.remove(0);
        let verb = BandingVerb::UpdateBanding {
            banded_range_id: 7,
            sheet: None,
            range: None,
            axis: BandingAxis::Columns,
            header_color: Some("#123456".to_string()),
            first_band_color: None,
            second_band_color: None,
            footer_color: None,
        };
        let (request, _) = build_request(&verb, None, Some(&existing)).unwrap();
        let BatchUpdateRequestItem::UpdateBanding(update) = request else {
            panic!("expected UpdateBanding"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns UpdateBanding for a BandingVerb::UpdateBanding verb"
        };
        assert_eq!(update.fields, "columnProperties");
        assert!(update.banded_range.row_properties.is_none());
        assert!(update.banded_range.column_properties.is_some());
    }

    #[test]
    fn build_request_update_banding_with_only_a_range_change_carries_no_properties() {
        let existing = sheet_with_banding(0, 7).banded_ranges.remove(0);
        let range = GridRange {
            sheet_id: 0,
            start_row_index: Some(5),
            end_row_index: Some(15),
            start_column_index: Some(0),
            end_column_index: Some(4),
        };
        let verb = BandingVerb::UpdateBanding {
            banded_range_id: 7,
            sheet: Some("Q1".to_string()),
            range: Some("A6:D15".to_string()),
            axis: BandingAxis::Rows,
            header_color: None,
            first_band_color: None,
            second_band_color: None,
            footer_color: None,
        };
        let (request, _) = build_request(&verb, Some(range), Some(&existing)).unwrap();
        let BatchUpdateRequestItem::UpdateBanding(update) = request else {
            panic!("expected UpdateBanding"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns UpdateBanding for a BandingVerb::UpdateBanding verb"
        };
        assert_eq!(update.fields, "range");
        assert_eq!(update.banded_range.range, Some(range));
        assert!(update.banded_range.row_properties.is_none());
    }

    #[test]
    fn build_request_delete_banding_carries_the_id() {
        let verb = BandingVerb::DeleteBanding { banded_range_id: 7 };
        let (request, existing_id) = build_request(&verb, None, None).unwrap();
        assert_eq!(existing_id, Some(7));
        let BatchUpdateRequestItem::DeleteBanding(delete) = request else {
            panic!("expected DeleteBanding"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns DeleteBanding for a BandingVerb::DeleteBanding verb"
        };
        assert_eq!(delete.banded_range_id, 7);
    }

    // ── end-to-end (wiremock) ────────────────────────────────────────────

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
                    "version": "1",
                })),
            )
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        mount_file(id, "application/vnd.google-apps.folder", &[])
    }

    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
    }

    fn allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsStructure).collect(),
            deny: HashSet::default(),
            require_lease: true,
        }
    }

    fn mount_workbook(sheets: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": sheets,
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn add_banding_dry_run_reports_would_change_and_makes_no_batch_update_call() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::WouldChange { .. }));
    }

    #[tokio::test]
    async fn add_banding_sends_an_add_banding_request_and_reports_the_assigned_id() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{"addBanding": {"bandedRange": {"bandedRangeId": 42}}}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert_eq!(
            outcome.result,
            BandingResult::Changed {
                summary: "add row banding".to_string(),
                banded_range_id: Some(42),
            }
        );
        assert_eq!(outcome.sheet_id, Some(0));
    }

    #[tokio::test]
    async fn update_banding_refuses_an_unknown_id() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}, "bandedRanges": []},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::UpdateBanding {
                banded_range_id: 99,
                sheet: None,
                range: None,
                axis: BandingAxis::Rows,
                header_color: Some("#000000".to_string()),
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert_eq!(
            outcome.result,
            BandingResult::RefusedBandedRangeNotFound {
                banded_range_id: 99
            }
        );
    }

    #[tokio::test]
    async fn update_banding_sends_an_update_banding_request_and_reports_the_id() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "bandedRanges": [{
                    "bandedRangeId": 7,
                    "range": {"sheetId": 0},
                    "rowProperties": {
                        "firstBandColorStyle": {"rgbColor": {"red": 1, "green": 1, "blue": 1}},
                        "secondBandColorStyle": {"rgbColor": {"red": 0, "green": 0, "blue": 0}},
                    },
                }],
            },
        ]))
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::UpdateBanding {
                banded_range_id: 7,
                sheet: None,
                range: None,
                axis: BandingAxis::Rows,
                header_color: Some("#000000".to_string()),
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert_eq!(
            outcome.result,
            BandingResult::Changed {
                summary: "update row banding id 7".to_string(),
                banded_range_id: Some(7),
            }
        );
        assert_eq!(outcome.sheet_id, Some(0));
    }

    #[tokio::test]
    async fn delete_banding_sends_a_delete_banding_request() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "bandedRanges": [{
                    "bandedRangeId": 7,
                    "range": {"sheetId": 0},
                    "rowProperties": {
                        "firstBandColorStyle": {"rgbColor": {"red": 1, "green": 1, "blue": 1}},
                        "secondBandColorStyle": {"rgbColor": {"red": 0, "green": 0, "blue": 0}},
                    },
                }],
            },
        ]))
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::DeleteBanding { banded_range_id: 7 },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert_eq!(
            outcome.result,
            BandingResult::Changed {
                summary: "delete banding id 7".to_string(),
                banded_range_id: Some(7),
            }
        );
        assert_eq!(outcome.sheet_id, Some(0));
    }

    // ── validate_verb/compose_target errors, reached through banding()
    // rather than by calling the pure helpers directly ─────────────────────

    #[tokio::test]
    async fn update_banding_end_to_end_refuses_nothing_to_change() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::UpdateBanding {
                banded_range_id: 1,
                sheet: None,
                range: None,
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn add_banding_end_to_end_rejects_conflicting_sheet_and_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Other".to_string(),
                range: "Sheet1!A1:B2".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedInvalidRange { .. }
        ));
    }

    // ── target_gate outcomes other than a granted gate ──────────────────

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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::Failed { .. }));
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::RefusedShortcut));
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedNotASpreadsheet { .. }
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedNoVisibleParents
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::Failed { .. }));
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
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::Failed { .. }));
    }

    // ── resolve_sheet_target's own error branches, reached through
    // banding() rather than by calling it directly ─────────────────────

    #[tokio::test]
    async fn add_banding_reports_sheet_not_found_for_an_unknown_sheet() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Missing".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedSheetNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn add_banding_reports_invalid_range_for_a_malformed_range() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "not-a-range".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn update_banding_reports_sheet_not_found_when_re_pointing() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::UpdateBanding {
                banded_range_id: 7,
                sheet: Some("Missing".to_string()),
                range: Some("A1:B2".to_string()),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedSheetNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn update_banding_reports_invalid_range_for_a_malformed_range_when_re_pointing() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::UpdateBanding {
                banded_range_id: 7,
                sheet: Some("Q1".to_string()),
                range: Some("not-a-range".to_string()),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn update_banding_with_a_new_range_succeeds() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "bandedRanges": [{
                    "bandedRangeId": 7,
                    "range": {"sheetId": 0},
                    "rowProperties": {
                        "firstBandColorStyle": {"rgbColor": {"red": 1, "green": 1, "blue": 1}},
                        "secondBandColorStyle": {"rgbColor": {"red": 0, "green": 0, "blue": 0}},
                    },
                }],
            },
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::UpdateBanding {
                banded_range_id: 7,
                sheet: Some("Q1".to_string()),
                range: Some("A1:B2".to_string()),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::WouldChange { .. }));
    }

    // ── build_request's own error branch, reached through banding()
    // rather than by calling build_request directly ─────────────────────

    #[tokio::test]
    async fn update_banding_end_to_end_rejects_a_malformed_color() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "bandedRanges": [{
                    "bandedRangeId": 7,
                    "range": {"sheetId": 0},
                    "rowProperties": {
                        "firstBandColorStyle": {"rgbColor": {"red": 1, "green": 1, "blue": 1}},
                        "secondBandColorStyle": {"rgbColor": {"red": 0, "green": 0, "blue": 0}},
                    },
                }],
            },
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::UpdateBanding {
                banded_range_id: 7,
                sheet: None,
                range: None,
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: Some("not-a-color".to_string()),
                second_band_color: None,
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedInvalidRange { .. }
        ));
    }

    // ── a real batchUpdate rejection surfaces as Failed ────────────────

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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::Failed { .. }));
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::RefusedNoLease));
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::RefusedLeaseExpired));
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "some-other-sheet", "1");
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            BandingResult::RefusedLeaseWrongFile
        ));
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let opts = BandingOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: BandingVerb::AddBanding {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                axis: BandingAxis::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
            },
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = banding(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, BandingResult::RefusedLeaseStale));
    }

    // ── describe/describe_lines (pure) ──────────────────────────────────

    fn outcome_with(
        verb: BandingVerb,
        file_name: Option<&str>,
        result: BandingResult,
    ) -> BandingOutcome {
        BandingOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: file_name.map(str::to_string),
            resolved_folder_id: None,
            sheet_id: None,
            verb,
            result,
        }
    }

    fn add_verb() -> BandingVerb {
        BandingVerb::AddBanding {
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            axis: BandingAxis::Rows,
            header_color: None,
            first_band_color: "#FFFFFF".to_string(),
            second_band_color: "#EEEEEE".to_string(),
            footer_color: None,
        }
    }

    fn update_verb() -> BandingVerb {
        BandingVerb::UpdateBanding {
            banded_range_id: 7,
            sheet: None,
            range: None,
            axis: BandingAxis::Rows,
            header_color: Some("#000000".to_string()),
            first_band_color: None,
            second_band_color: None,
            footer_color: None,
        }
    }

    fn delete_verb() -> BandingVerb {
        BandingVerb::DeleteBanding { banded_range_id: 7 }
    }

    #[test]
    fn describe_lines_renders_would_change() {
        let out = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::WouldChange {
                summary: "add row banding".to_string(),
            },
        );
        assert_eq!(describe(&out), "Would add row banding in 'Budget'");
    }

    #[test]
    fn describe_lines_renders_not_a_spreadsheet_with_no_file_name() {
        let out = outcome_with(
            add_verb(),
            None,
            BandingResult::RefusedNotASpreadsheet {
                mime_type: "text/plain".to_string(),
            },
        );
        let text = describe(&out);
        assert!(text.contains("'sheet-1'"), "{text}");
        assert!(text.contains("add-banding"), "{text}");
        assert!(text.contains("text/plain"), "{text}");
    }

    #[test]
    fn describe_lines_renders_shortcut() {
        let out = outcome_with(
            delete_verb(),
            Some("Budget"),
            BandingResult::RefusedShortcut,
        );
        let text = describe(&out);
        assert!(text.contains("shortcut"), "{text}");
        assert!(text.contains("delete-banding"), "{text}");
    }

    #[test]
    fn describe_lines_renders_no_visible_parents() {
        let out = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::RefusedNoVisibleParents,
        );
        assert!(describe(&out).contains("sheets-structure"));
    }

    #[test]
    fn describe_lines_renders_sheet_not_found_with_and_without_available_titles() {
        let none = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::RefusedSheetNotFound {
                title: "Q2".to_string(),
                available: Vec::new(),
            },
        );
        assert!(describe(&none).contains("Available: none"));

        let some = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::RefusedSheetNotFound {
                title: "Q2".to_string(),
                available: vec!["Q1".to_string(), "Q3".to_string()],
            },
        );
        assert!(describe(&some).contains("'Q1', 'Q3'"));
    }

    #[test]
    fn describe_lines_renders_invalid_range_and_banded_range_not_found() {
        let invalid = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
        );
        assert_eq!(describe(&invalid), "Refused: bad range");

        let not_found = outcome_with(
            update_verb(),
            Some("Budget"),
            BandingResult::RefusedBandedRangeNotFound {
                banded_range_id: 99,
            },
        );
        let text = describe(&not_found);
        assert!(text.contains("no banded range with id 99"), "{text}");
        assert!(text.contains("list-bandings"), "{text}");
    }

    #[test]
    fn describe_lines_renders_blocked_with_and_without_a_deciding_rule() {
        let folder_rule = outcome_with(
            update_verb(),
            Some("Budget"),
            BandingResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
        );
        let text = describe(&folder_rule);
        assert!(text.contains("update-banding"), "{text}");
        assert!(text.contains("folder folder-1 (depth 2)"), "{text}");

        let default_policy = outcome_with(
            delete_verb(),
            Some("Budget"),
            BandingResult::Blocked { decided_by: None },
        );
        assert!(describe(&default_policy).contains("default policy"));
    }

    #[test]
    fn describe_lines_renders_every_lease_refusal() {
        for (result, needle) in [
            (
                BandingResult::RefusedNoLease,
                "requires a Drive write lease",
            ),
            (
                BandingResult::RefusedLeaseExpired,
                "expired, released, or unknown",
            ),
            (
                BandingResult::RefusedLeaseWrongFile,
                "acquired for a different file",
            ),
            (
                BandingResult::RefusedLeaseStale,
                "changed since the lease was acquired",
            ),
        ] {
            let out = outcome_with(add_verb(), Some("Budget"), result);
            let text = describe(&out);
            assert!(text.contains(needle), "{text}");
            assert!(text.contains("drive lease acquire sheet-1"), "{text}");
        }
    }

    #[test]
    fn describe_lines_renders_changed_with_and_without_an_id() {
        let with_id = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::Changed {
                summary: "add row banding".to_string(),
                banded_range_id: Some(42),
            },
        );
        assert!(describe(&with_id).contains("(id 42)"));

        let without_id = outcome_with(
            update_verb(),
            Some("Budget"),
            BandingResult::Changed {
                summary: "update row banding id 7".to_string(),
                banded_range_id: None,
            },
        );
        let text = describe(&without_id);
        assert!(!text.contains("(id"), "{text}");
    }

    #[test]
    fn describe_lines_renders_failed() {
        let out = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::Failed {
                detail: "boom".to_string(),
            },
        );
        assert_eq!(describe(&out), "Failed: boom");
    }

    // ── write_jsonl / log_status ─────────────────────────────────────────

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = outcome_with(
            add_verb(),
            Some("Budget"),
            BandingResult::Changed {
                summary: "add row banding".to_string(),
                banded_range_id: Some(1),
            },
        );
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "changed");
    }

    #[test]
    fn banding_result_log_status_names_every_variant() {
        assert_eq!(
            BandingResult::WouldChange {
                summary: String::new(),
            }
            .log_status(),
            "would-change"
        );
        assert_eq!(
            BandingResult::RefusedNotASpreadsheet {
                mime_type: String::new(),
            }
            .log_status(),
            "refused-not-a-spreadsheet"
        );
        assert_eq!(
            BandingResult::RefusedShortcut.log_status(),
            "refused-shortcut"
        );
        assert_eq!(
            BandingResult::RefusedNoVisibleParents.log_status(),
            "refused-no-visible-parents"
        );
        assert_eq!(
            BandingResult::RefusedSheetNotFound {
                title: String::new(),
                available: Vec::new(),
            }
            .log_status(),
            "refused-sheet-not-found"
        );
        assert_eq!(
            BandingResult::RefusedInvalidRange {
                detail: String::new(),
            }
            .log_status(),
            "refused-invalid-range"
        );
        assert_eq!(
            BandingResult::RefusedBandedRangeNotFound { banded_range_id: 1 }.log_status(),
            "refused-banded-range-not-found"
        );
        assert_eq!(
            BandingResult::Blocked { decided_by: None }.log_status(),
            "blocked"
        );
        assert_eq!(
            BandingResult::RefusedNoLease.log_status(),
            LeaseGateRefusal::NoLease.log_status()
        );
        assert_eq!(
            BandingResult::RefusedLeaseExpired.log_status(),
            LeaseGateRefusal::Expired.log_status()
        );
        assert_eq!(
            BandingResult::RefusedLeaseWrongFile.log_status(),
            LeaseGateRefusal::WrongFile.log_status()
        );
        assert_eq!(
            BandingResult::RefusedLeaseStale.log_status(),
            LeaseGateRefusal::Stale.log_status()
        );
        assert_eq!(
            BandingResult::Changed {
                summary: String::new(),
                banded_range_id: None,
            }
            .log_status(),
            "changed"
        );
        assert_eq!(
            BandingResult::Failed {
                detail: String::new(),
            }
            .log_status(),
            "failed"
        );
    }
}
