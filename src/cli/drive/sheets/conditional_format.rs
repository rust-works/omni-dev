//! CLI commands for `omni-dev drive sheets add-conditional-format`/
//! `update-conditional-format`/`delete-conditional-format`/
//! `list-conditional-formats` (issue #1793).
//!
//! The first three are gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! (ADR-0081 §1) — presentational, destroys no data. `list-conditional-formats`
//! is a plain read, ungated like `list-protections`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::conditional_format::{
    conditional_format, describe_existing_rule, describe_lines, ConditionalFormatOptions,
    ConditionalFormatVerb, FormatCondition, FormatEffect, FormatRule, GradientMidpoint,
    GradientPointType, GradientSpec,
};
use crate::drive::sheets::date_value::DateValue;

/// The `--gradient-mid-type` values — mirrors [`GradientPointType`] with a
/// `clap::ValueEnum` derive, since the engine type deliberately carries no
/// `clap` dependency (the same split `crate::cli::drive::permissions::check`
/// uses for `DriveOperation`).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum GradientMidTypeArg {
    /// Anchored at an absolute cell value.
    Number,
    /// Anchored at a percentage of the range's value spread.
    Percent,
    /// Anchored at a percentile of the range's values.
    Percentile,
}

impl From<GradientMidTypeArg> for GradientPointType {
    fn from(value: GradientMidTypeArg) -> Self {
        match value {
            GradientMidTypeArg::Number => Self::Number,
            GradientMidTypeArg::Percent => Self::Percent,
            GradientMidTypeArg::Percentile => Self::Percentile,
        }
    }
}

/// The condition/gradient flags shared by `add-conditional-format` and
/// `update-conditional-format` — factored out so both leaves declare,
/// document and select from exactly the same set.
///
/// Exactly one condition flag, or `--gradient-min-color` (which brings its
/// own companion flags along via `requires`), is required — enforced by the
/// `rule_kind` `ArgGroup` on each command that flattens this.
#[derive(Parser)]
pub struct ConditionalFormatRuleArgs {
    /// Restrict the trigger to a number in this inclusive range.
    #[arg(long, num_args = 2, value_names = ["MIN", "MAX"], allow_hyphen_values = true)]
    pub number_between: Option<Vec<f64>>,

    /// Restrict the trigger to a number outside this inclusive range.
    #[arg(long, num_args = 2, value_names = ["MIN", "MAX"], allow_hyphen_values = true)]
    pub number_not_between: Option<Vec<f64>>,

    /// Restrict the trigger to a number strictly greater than this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_greater: Option<f64>,

    /// Restrict the trigger to a number greater than or equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_greater_eq: Option<f64>,

    /// Restrict the trigger to a number strictly less than this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_less: Option<f64>,

    /// Restrict the trigger to a number less than or equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_less_eq: Option<f64>,

    /// Restrict the trigger to a number equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_eq: Option<f64>,

    /// Restrict the trigger to a number not equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_not_eq: Option<f64>,

    /// Restrict the trigger to text containing this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_contains: Option<String>,

    /// Restrict the trigger to text not containing this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_not_contains: Option<String>,

    /// Restrict the trigger to text starting with this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_starts_with: Option<String>,

    /// Restrict the trigger to text ending with this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_ends_with: Option<String>,

    /// Restrict the trigger to text equal to this.
    #[arg(long, value_name = "TEXT")]
    pub text_eq: Option<String>,

    /// Restrict the trigger to a date after this one. Accepts an absolute
    /// date or a relative keyword (`today`, `tomorrow`, `yesterday`,
    /// `past-week`, `past-month`, `past-year`).
    #[arg(long, value_name = "DATE")]
    pub date_after: Option<String>,

    /// Restrict the trigger to a date before this one. Accepts an absolute
    /// date or a relative keyword (see `--date-after`).
    #[arg(long, value_name = "DATE")]
    pub date_before: Option<String>,

    /// Restrict the trigger to a date equal to this one. Accepts an
    /// absolute date or a relative keyword (see `--date-after`).
    #[arg(long, value_name = "DATE")]
    pub date_on: Option<String>,

    /// Restrict the trigger to a date in this inclusive range (absolute
    /// dates only; relative keywords are not accepted here).
    #[arg(long, num_args = 2, value_names = ["START", "END"])]
    pub date_between: Option<Vec<String>>,

    /// Trigger when the cell is empty.
    #[arg(long)]
    pub cell_empty: bool,

    /// Trigger when the cell is not empty.
    #[arg(long)]
    pub cell_not_empty: bool,

    /// Trigger when this formula evaluates truthy.
    #[arg(long, value_name = "FORMULA")]
    pub custom_formula: Option<String>,

    /// Cell background color to apply, `#RRGGBB`. Companion to a condition
    /// flag above, not part of the `rule_kind` group itself.
    #[arg(long, value_name = "HEX")]
    pub background: Option<String>,

    /// Text color to apply, `#RRGGBB`.
    #[arg(long, value_name = "HEX")]
    pub text_color: Option<String>,

    /// Bold text.
    #[arg(long, value_name = "BOOL")]
    pub bold: Option<bool>,

    /// Build a gradient instead: the color at the low end of the scale
    /// (anchored `MIN`), `#RRGGBB`. Requires `--gradient-max-color`.
    #[arg(long, value_name = "HEX", requires = "gradient_max_color")]
    pub gradient_min_color: Option<String>,

    /// The color at the high end of the scale (anchored `MAX`), `#RRGGBB`.
    #[arg(long, value_name = "HEX", requires = "gradient_min_color")]
    pub gradient_max_color: Option<String>,

    /// An optional midpoint's color, `#RRGGBB`. Requires
    /// `--gradient-mid-type`/`--gradient-mid-value`.
    #[arg(
        long,
        value_name = "HEX",
        requires_all = ["gradient_mid_type", "gradient_mid_value"]
    )]
    pub gradient_mid_color: Option<String>,

    /// What kind of value the midpoint is anchored to.
    #[arg(long, value_enum, value_name = "TYPE", requires = "gradient_mid_color")]
    pub gradient_mid_type: Option<GradientMidTypeArg>,

    /// The midpoint's threshold value, untouched — Sheets parses it at
    /// evaluation time.
    #[arg(long, value_name = "VALUE", requires = "gradient_mid_color")]
    pub gradient_mid_value: Option<String>,
}

impl ConditionalFormatRuleArgs {
    /// clap arg ids for the `rule_kind` `ArgGroup`: every condition flag
    /// plus `gradient_min_color`, which pulls its gradient companions in
    /// via `requires` rather than listing them here too.
    const RULE_KIND_ARGS: &'static [&'static str] = &[
        "number_between",
        "number_not_between",
        "number_greater",
        "number_greater_eq",
        "number_less",
        "number_less_eq",
        "number_eq",
        "number_not_eq",
        "text_contains",
        "text_not_contains",
        "text_starts_with",
        "text_ends_with",
        "text_eq",
        "date_after",
        "date_before",
        "date_on",
        "date_between",
        "cell_empty",
        "cell_not_empty",
        "custom_formula",
        "gradient_min_color",
    ];

    /// Selects the rule the `rule_kind` `ArgGroup` guarantees is set.
    /// `--custom-formula` is the unconditional final fallback among the
    /// boolean-condition flags, mirroring `validation.rs`'s
    /// `select_condition`.
    fn select_rule(&self) -> FormatRule {
        if let Some(min) = &self.gradient_min_color {
            let mid = self
                .gradient_mid_color
                .as_ref()
                .map(|color| GradientMidpoint {
                    color: color.clone(),
                    // `requires_all` on `gradient_mid_color` guarantees both are
                    // set whenever it is.
                    point_type: self
                        .gradient_mid_type
                        .map_or(GradientPointType::Number, GradientPointType::from),
                    value: self.gradient_mid_value.clone().unwrap_or_default(),
                });
            return FormatRule::Gradient(GradientSpec {
                min_color: min.clone(),
                // `requires = "gradient_max_color"` on `gradient_min_color`
                // guarantees this is set.
                max_color: self.gradient_max_color.clone().unwrap_or_default(),
                mid,
            });
        }

        let condition = if let Some(v) = &self.number_between {
            FormatCondition::NumberBetween(v[0], v[1])
        } else if let Some(v) = &self.number_not_between {
            FormatCondition::NumberNotBetween(v[0], v[1])
        } else if let Some(n) = self.number_greater {
            FormatCondition::NumberGreater(n)
        } else if let Some(n) = self.number_greater_eq {
            FormatCondition::NumberGreaterEq(n)
        } else if let Some(n) = self.number_less {
            FormatCondition::NumberLess(n)
        } else if let Some(n) = self.number_less_eq {
            FormatCondition::NumberLessEq(n)
        } else if let Some(n) = self.number_eq {
            FormatCondition::NumberEq(n)
        } else if let Some(n) = self.number_not_eq {
            FormatCondition::NumberNotEq(n)
        } else if let Some(text) = &self.text_contains {
            FormatCondition::TextContains(text.clone())
        } else if let Some(text) = &self.text_not_contains {
            FormatCondition::TextNotContains(text.clone())
        } else if let Some(text) = &self.text_starts_with {
            FormatCondition::TextStartsWith(text.clone())
        } else if let Some(text) = &self.text_ends_with {
            FormatCondition::TextEndsWith(text.clone())
        } else if let Some(text) = &self.text_eq {
            FormatCondition::TextEq(text.clone())
        } else if let Some(raw) = &self.date_after {
            FormatCondition::DateAfter(DateValue::parse(raw.clone()))
        } else if let Some(raw) = &self.date_before {
            FormatCondition::DateBefore(DateValue::parse(raw.clone()))
        } else if let Some(raw) = &self.date_on {
            FormatCondition::DateOn(DateValue::parse(raw.clone()))
        } else if let Some(v) = &self.date_between {
            FormatCondition::DateBetween(v[0].clone(), v[1].clone())
        } else if self.cell_empty {
            FormatCondition::CellEmpty
        } else if self.cell_not_empty {
            FormatCondition::CellNotEmpty
        } else {
            FormatCondition::CustomFormula(self.custom_formula.clone().unwrap_or_default())
        };

        FormatRule::Boolean {
            condition,
            format: FormatEffect {
                background: self.background.clone(),
                text_color: self.text_color.clone(),
                bold: self.bold,
            },
        }
    }
}

/// Adds a conditional format rule to one or more ranges on a sheet.
///
/// Exactly one condition flag is required for a `BooleanRule` (the numeric
/// comparators, the text conditions, the date conditions, `--cell-empty`/
/// `--cell-not-empty`, or `--custom-formula`), or `--gradient-min-color` for
/// a `GradientRule`. Unlike `set-data-validation`, `--sheet` is required:
/// a rule's ranges must all share one sheet, and requiring it up front
/// avoids resolving each `--range`'s own optional prefix against a
/// possibly-different sheet.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("rule_kind")
    .args(ConditionalFormatRuleArgs::RULE_KIND_ARGS)
    .required(true)))]
pub struct AddConditionalFormatCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title every `--range` belongs to.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// An A1 range (no `Sheet!` prefix — `--sheet` supplies it) the rule
    /// applies to. Repeatable; at least one is required.
    #[arg(long = "range", value_name = "A1", required = true)]
    pub ranges: Vec<String>,

    /// Where to insert the rule, 0-based. Omit to append (the sheet's
    /// current rule count).
    #[arg(long, value_name = "N")]
    pub index: Option<i64>,

    #[command(flatten)]
    pub rule: ConditionalFormatRuleArgs,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AddConditionalFormatCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = ConditionalFormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ConditionalFormatVerb::AddConditionalFormat {
                sheet: self.sheet,
                ranges: self.ranges,
                index: self.index,
                rule: self.rule.select_rule(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_conditional_format(client, &opts, &self.output).await
    }
}

/// Replaces the rule at an index. Use `list-conditional-formats` to find
/// the index and confirm it's still current — deleting an earlier rule
/// shifts every later one.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("rule_kind")
    .args(ConditionalFormatRuleArgs::RULE_KIND_ARGS)
    .required(true)))]
pub struct UpdateConditionalFormatCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title the rule belongs to.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Which rule, by 0-based ordinal position, to replace. See
    /// `list-conditional-formats`.
    #[arg(long, value_name = "N")]
    pub index: usize,

    /// The rule's new range(s) (no `Sheet!` prefix — `--sheet` supplies
    /// it). Repeatable; at least one is required. The whole rule is
    /// replaced, ranges included — this is not an incremental edit.
    #[arg(long = "range", value_name = "A1", required = true)]
    pub ranges: Vec<String>,

    #[command(flatten)]
    pub rule: ConditionalFormatRuleArgs,

    /// Reports the gate verdict and the change that would be made
    /// (including the rule currently at `--index`), without calling
    /// `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UpdateConditionalFormatCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = ConditionalFormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ConditionalFormatVerb::UpdateConditionalFormat {
                sheet: self.sheet,
                ranges: self.ranges,
                index: self.index,
                rule: self.rule.select_rule(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_conditional_format(client, &opts, &self.output).await
    }
}

/// Removes the rule at an index. Use `list-conditional-formats` to find the
/// index and confirm it's still current — deleting an earlier rule shifts
/// every later one.
#[derive(Parser)]
pub struct DeleteConditionalFormatCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title the rule belongs to.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Which rule, by 0-based ordinal position, to remove. See
    /// `list-conditional-formats`.
    #[arg(long, value_name = "N")]
    pub index: usize,

    /// Reports the gate verdict and the rule currently at `--index`,
    /// without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeleteConditionalFormatCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = ConditionalFormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ConditionalFormatVerb::DeleteConditionalFormat {
                sheet: self.sheet,
                index: self.index,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_conditional_format(client, &opts, &self.output).await
    }
}

/// Lists the conditional format rules in a spreadsheet.
///
/// Read-only and ungated, like `list-protections` — needed so
/// `update-conditional-format`/`delete-conditional-format` are usable at
/// all, since a rule's ordinal index (and current content) is otherwise
/// invisible from the CLI, and that index shifts whenever an earlier rule
/// is deleted.
#[derive(Parser)]
pub struct ListConditionalFormatsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListConditionalFormatsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_conditional_formats(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for sheet in &workbook.sheets {
            for (index, rule) in sheet.conditional_formats.iter().enumerate() {
                println!(
                    "{}",
                    sanitize_for_terminal(&format!(
                        "index {index}: {}  sheet={}",
                        describe_existing_rule(rule),
                        sheet.title()
                    ))
                );
            }
        }
        Ok(())
    }
}

async fn run_conditional_format(
    client: &DriveClient,
    opts: &ConditionalFormatOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = conditional_format(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let lines: Vec<String> = describe_lines(&outcome)
        .into_iter()
        .map(|line| sanitize_for_terminal(&line))
        .collect();
    println!("{}", lines.join("\n"));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse_add(args: &[&str]) -> AddConditionalFormatCommand {
        let mut full = vec!["add-conditional-format"];
        full.extend_from_slice(args);
        AddConditionalFormatCommand::parse_from(full)
    }

    #[test]
    fn add_command_debug_asserts_its_clap_structure() {
        AddConditionalFormatCommand::command().debug_assert();
        UpdateConditionalFormatCommand::command().debug_assert();
        DeleteConditionalFormatCommand::command().debug_assert();
        ListConditionalFormatsCommand::command().debug_assert();
    }

    #[test]
    fn select_rule_picks_a_boolean_condition() {
        let cmd = parse_add(&[
            "sheet-1",
            "--sheet",
            "Q1",
            "--range",
            "A1:A10",
            "--number-greater",
            "100",
            "--background",
            "#00FF00",
        ]);
        match cmd.rule.select_rule() {
            FormatRule::Boolean { condition, format } => {
                match condition {
                    FormatCondition::NumberGreater(n) => assert!((n - 100.0).abs() < f64::EPSILON),
                    other => panic!("expected NumberGreater, got {other:?}"),
                }
                assert_eq!(format.background, Some("#00FF00".to_string()));
            }
            other @ FormatRule::Gradient(_) => panic!("expected Boolean, got {other:?}"),
        }
    }

    #[test]
    fn select_rule_picks_cell_empty() {
        let cmd = parse_add(&[
            "sheet-1",
            "--sheet",
            "Q1",
            "--range",
            "A1:A10",
            "--cell-empty",
            "--bold",
            "true",
        ]);
        match cmd.rule.select_rule() {
            FormatRule::Boolean { condition, format } => {
                assert!(matches!(condition, FormatCondition::CellEmpty));
                assert_eq!(format.bold, Some(true));
            }
            other @ FormatRule::Gradient(_) => panic!("expected Boolean, got {other:?}"),
        }
    }

    #[test]
    fn select_rule_picks_a_gradient_with_a_midpoint() {
        let cmd = parse_add(&[
            "sheet-1",
            "--sheet",
            "Q1",
            "--range",
            "A1:A10",
            "--gradient-min-color",
            "#FFFFFF",
            "--gradient-max-color",
            "#00FF00",
            "--gradient-mid-color",
            "#FFFF00",
            "--gradient-mid-type",
            "percent",
            "--gradient-mid-value",
            "50",
        ]);
        match cmd.rule.select_rule() {
            FormatRule::Gradient(spec) => {
                assert_eq!(spec.min_color, "#FFFFFF");
                assert_eq!(spec.max_color, "#00FF00");
                let mid = spec.mid.unwrap();
                assert_eq!(mid.color, "#FFFF00");
                assert!(matches!(mid.point_type, GradientPointType::Percent));
                assert_eq!(mid.value, "50");
            }
            other @ FormatRule::Boolean { .. } => panic!("expected Gradient, got {other:?}"),
        }
    }

    #[test]
    fn add_requires_exactly_one_rule_kind_flag() {
        let err = AddConditionalFormatCommand::try_parse_from([
            "add-conditional-format",
            "sheet-1",
            "--sheet",
            "Q1",
            "--range",
            "A1:A10",
        ])
        .map(|_| ())
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn add_rejects_both_a_condition_and_a_gradient_flag() {
        let err = AddConditionalFormatCommand::try_parse_from([
            "add-conditional-format",
            "sheet-1",
            "--sheet",
            "Q1",
            "--range",
            "A1:A10",
            "--cell-empty",
            "--gradient-min-color",
            "#FFFFFF",
            "--gradient-max-color",
            "#00FF00",
        ])
        .map(|_| ())
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn add_requires_at_least_one_range() {
        let err = AddConditionalFormatCommand::try_parse_from([
            "add-conditional-format",
            "sheet-1",
            "--sheet",
            "Q1",
            "--cell-empty",
        ])
        .map(|_| ())
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn gradient_min_color_requires_max_color() {
        let err = AddConditionalFormatCommand::try_parse_from([
            "add-conditional-format",
            "sheet-1",
            "--sheet",
            "Q1",
            "--range",
            "A1:A10",
            "--gradient-min-color",
            "#FFFFFF",
        ])
        .map(|_| ())
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn delete_command_needs_no_rule_kind_flag() {
        let cmd = DeleteConditionalFormatCommand::try_parse_from([
            "delete-conditional-format",
            "sheet-1",
            "--sheet",
            "Q1",
            "--index",
            "0",
        ])
        .unwrap();
        assert_eq!(cmd.index, 0);
    }
}
