//! Gated Docs/Sheets content writes and lease acquisition over MCP (ADR-0091).
//!
//! Handlers delegate to the CLI's engines. Gates, backup leases, freshness,
//! locking and audit records belong to those engines, never this transport.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use super::{error::tool_error, server::OmniDevServer};
use crate::cli::drive::{helpers, lease::LeaseFlags, sheets::values::ValuesFormat};
use crate::drive::{
    client::DriveClient,
    docs::{client::DocsClient, write as docs_write},
    lease::{
        acquire::{self, AcquireOptions, AcquireResult},
        authenticate,
    },
    sheets::{api::ValueInputOption, client::SheetsClient, write as sheets_write},
};
use crate::mcp::drive_tools::account_param_doc;

/// Parameters for DocsReplace operations.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveDocsReplaceParams {
    /// Document id from /d/<ID>/ in a Docs URL, e.g. 1a2B3c4D. Required.
    pub document_id: String,
    /// Literal text to find, e.g. draft; not a regular expression. Required.
    pub search: String,
    /// Replacement text, e.g. final. Empty deletes matches. Required.
    pub replace: String,
    /// Case-insensitive search. Default false (case-sensitive).
    #[serde(default)]
    pub ignore_case: Option<bool>,
    /// Preview without mutating or requiring a lease. Default false; preview first.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Token from drive_lease_acquire or omni-dev drive lease acquire. Not needed for
    /// dry_run or operator require_lease:false rules; any supplied token is still
    /// validated.
    #[serde(default)]
    pub lease: Option<String>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for DocsAppend operations.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveDocsAppendParams {
    /// Document id from /d/<ID>/ in a Docs URL, e.g. 1a2B3c4D. Required.
    pub document_id: String,
    /// Text to append, e.g. a new paragraph. Exactly one of text or text_path is required.
    #[serde(default)]
    pub text: Option<String>,
    /// Local UTF-8 text file, mutually exclusive with text. Stdin (-) is unsupported.
    #[serde(default)]
    pub text_path: Option<String>,
    /// Preview without mutating or requiring a lease. Default false; preview first.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Token from drive_lease_acquire or omni-dev drive lease acquire. Not needed for
    /// dry_run or operator require_lease:false rules; any supplied token is still
    /// validated.
    #[serde(default)]
    pub lease: Option<String>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for SheetsWrite operations.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveSheetsWriteParams {
    /// Spreadsheet id from /d/<ID>/ in a Sheets URL, e.g. 1a2B3c4D. Required.
    pub spreadsheet_id: String,
    /// A1 range, e.g. A1:B2 or Sheet1!A1:B2. Omit with sheet to target the tab.
    #[serde(default)]
    pub range: Option<String>,
    /// Tab title, e.g. Sheet1. Supplies a bare range prefix; conflicts with a range already
    /// naming a tab.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Inline array of rows, e.g. `[["name", "score"], ["Ada", "42"]]`. Exactly one of values
    /// or values_path is required.
    #[serde(default)]
    pub values: Option<Vec<Vec<String>>>,
    /// Local UTF-8 CSV/TSV/JSON file; mutually exclusive with values. Stdin (-) is unsupported.
    #[serde(default)]
    pub values_path: Option<String>,
    /// File format: auto (default; infer extension), csv, tsv, json. Only applies to values_path.
    #[serde(default)]
    pub values_format: Option<String>,
    /// Interpretation: user-entered (default; parses formulas/dates/numbers) or raw (literal text).
    #[serde(default)]
    pub input: Option<String>,
    /// Preview without mutating or requiring a lease. Default false; preview first.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Token from drive_lease_acquire or omni-dev drive lease acquire. Not needed for
    /// dry_run or operator require_lease:false rules; any supplied token is still
    /// validated.
    #[serde(default)]
    pub lease: Option<String>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for SheetsClear operations.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveSheetsClearParams {
    /// Spreadsheet id from /d/<ID>/ in a Sheets URL, e.g. 1a2B3c4D. Required.
    pub spreadsheet_id: String,
    /// A1 range, e.g. A1:B2 or Sheet1!A1:B2. Omit with sheet to target the tab.
    #[serde(default)]
    pub range: Option<String>,
    /// Tab title, e.g. Sheet1. Supplies a bare range prefix; conflicts with a range already
    /// naming a tab.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Preview without mutating or requiring a lease. Default false; preview first.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Token from drive_lease_acquire or omni-dev drive lease acquire. Not needed for
    /// dry_run or operator require_lease:false rules; any supplied token is still
    /// validated.
    #[serde(default)]
    pub lease: Option<String>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for LeaseAcquire operations.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveLeaseAcquireParams {
    /// Drive file id to back up and lease, e.g. 1a2B3c4D. Required.
    pub file_id: String,
    /// Lease lifetime in minutes (1–1440). Defaults to operator env/settings, then 30;
    /// writes never extend it.
    #[serde(default)]
    pub expiry_minutes: Option<i64>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

#[allow(missing_docs)] // tool_router generates the public router.
#[tool_router(router = drive_write_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Replace literal text in a Google Doc; use drive_docs_append to add text at the end.
    #[tool(
        description = "Replace literal text in a Google Doc; use drive_docs_append to add text at the end. \
                       Mirrors `omni-dev drive docs replace`. Case-sensitive by default. Example: \
                       document_id, search:\"draft\", replace:\"final\", dry_run:true. occurrences is a \
                       body-only estimate excluding headers/footers/footnotes; zero still sends a real \
                       request, and occurrences_changed is authoritative. stale-revision means reread and \
                       retry through the complete engine, which may then refuse a stale lease. dry_run \
                       defaults to false. Dry-run first: previews need no lease. Real writes require an \
                       operator allow rule and a lease unless require_lease:false. Acquire with \
                       drive_lease_acquire. A lease permits repeated writes to one file until expiry. \
                       refused-lease-stale needs a fresh backup: release with omni-dev drive lease release \
                       or wait for expiry before acquiring (acquire reuses stale live tokens too). \
                       Cancellation/timeout is not rollback; inspect state before retrying, especially \
                       append. Output is complete tagged YAML; refusals/failures set is_error."
    )]
    pub async fn drive_docs_replace(
        &self,
        Parameters(params): Parameters<DriveDocsReplaceParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = helpers::create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let payload = docs_write::WritePayload::Replace {
            search: params.search.clone(),
            replace: params.replace.clone(),
            match_case: !params.ignore_case.unwrap_or(false),
        };
        run_docs_write(
            &client,
            &DocsClient::from_drive_client(&client).map_err(tool_error)?,
            &params.document_id,
            payload,
            params.dry_run.unwrap_or(false),
            params.lease.clone(),
            params.account.as_deref(),
        )
        .await
        .map_err(tool_error)
    }

    /// Append text at the end of a Google Doc; use drive_docs_replace to change existing text.
    #[tool(
        description = "Append text at the end of a Google Doc; use drive_docs_replace to change existing \
                       text. Mirrors `omni-dev drive docs append`. Example: document_id, text:\"new \
                       paragraph\", dry_run:true. Use text_path for existing files. stale-revision means \
                       reread and retry through the complete engine, which may then refuse a stale lease. \
                       dry_run defaults to false. Dry-run first: previews need no lease. Real writes require \
                       an operator allow rule and a lease unless require_lease:false. Acquire with \
                       drive_lease_acquire. A lease permits repeated writes to one file until expiry. \
                       refused-lease-stale needs a fresh backup: release with omni-dev drive lease release \
                       or wait for expiry before acquiring (acquire reuses stale live tokens too). \
                       Cancellation/timeout is not rollback; inspect state before retrying, especially \
                       append. Output is complete tagged YAML; refusals/failures set is_error."
    )]
    pub async fn drive_docs_append(
        &self,
        Parameters(params): Parameters<DriveDocsAppendParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = helpers::create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let input = params.clone();
        let text = tokio::task::spawn_blocking(move || append_text(&input))
            .await
            .context("Text input task failed")
            .map_err(tool_error)?
            .map_err(tool_error)?;
        run_docs_write(
            &client,
            &DocsClient::from_drive_client(&client).map_err(tool_error)?,
            &params.document_id,
            docs_write::WritePayload::Append { text },
            params.dry_run.unwrap_or(false),
            params.lease.clone(),
            params.account.as_deref(),
        )
        .await
        .map_err(tool_error)
    }

    /// Overwrite cell values in a Google Sheet, dropping rich-text runs; use
    /// drive_sheets_append to add table rows or drive_sheets_clear to empty values.
    #[tool(
        description = "Overwrite cell values in a Google Sheet, dropping rich-text runs; use \
                       drive_sheets_append to add table rows or drive_sheets_clear to empty values. Mirrors \
                       `omni-dev drive sheets write`. Example: spreadsheet_id, range:\"A1:B2\", \
                       values:[[\"a\",\"b\"]], dry_run:true. dry_run defaults to false. Dry-run first: previews \
                       need no lease. Real writes require an operator allow rule and a lease unless \
                       require_lease:false. Acquire with drive_lease_acquire. A lease permits repeated \
                       writes to one file until expiry. refused-lease-stale needs a fresh backup: release \
                       with omni-dev drive lease release or wait for expiry before acquiring (acquire reuses \
                       stale live tokens too). Cancellation/timeout is not rollback; inspect state before \
                       retrying, especially append. Output is complete tagged YAML; refusals/failures set \
                       is_error."
    )]
    pub async fn drive_sheets_write(
        &self,
        Parameters(params): Parameters<DriveSheetsWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = helpers::create_client_for(params.account.as_deref()).map_err(tool_error)?;
        run_sheets_write(
            &client,
            &SheetsClient::from_drive_client(&client).map_err(tool_error)?,
            &params,
            sheets_write::WriteVerb::Write,
        )
        .await
        .map_err(tool_error)
    }

    /// Append rows after the table in a Google Sheet range; use drive_sheets_write to
    /// overwrite existing cells.
    #[tool(
        description = "Append rows after the table in a Google Sheet range; use drive_sheets_write to \
                       overwrite existing cells. Mirrors `omni-dev drive sheets append`. Example: \
                       spreadsheet_id, range:\"A1:B2\", values:[[\"a\",\"b\"]], dry_run:true. dry_run defaults to \
                       false. Dry-run first: previews need no lease. Real writes require an operator allow \
                       rule and a lease unless require_lease:false. Acquire with drive_lease_acquire. A \
                       lease permits repeated writes to one file until expiry. refused-lease-stale needs a \
                       fresh backup: release with omni-dev drive lease release or wait for expiry before \
                       acquiring (acquire reuses stale live tokens too). Cancellation/timeout is not \
                       rollback; inspect state before retrying, especially append. Output is complete tagged \
                       YAML; refusals/failures set is_error."
    )]
    pub async fn drive_sheets_append(
        &self,
        Parameters(params): Parameters<DriveSheetsWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = helpers::create_client_for(params.account.as_deref()).map_err(tool_error)?;
        run_sheets_write(
            &client,
            &SheetsClient::from_drive_client(&client).map_err(tool_error)?,
            &params,
            sheets_write::WriteVerb::Append,
        )
        .await
        .map_err(tool_error)
    }

    /// Clear cell values while retaining formatting; use drive_sheets_write to replace them.
    #[tool(
        description = "Clear cell values while retaining formatting; use drive_sheets_write to replace them. \
                       Mirrors `omni-dev drive sheets clear`. Example: spreadsheet_id, range:\"A1:B2\", \
                       dry_run:true. dry_run defaults to false. Dry-run first: previews need no lease. Real \
                       writes require an operator allow rule and a lease unless require_lease:false. Acquire \
                       with drive_lease_acquire. A lease permits repeated writes to one file until expiry. \
                       refused-lease-stale needs a fresh backup: release with omni-dev drive lease release \
                       or wait for expiry before acquiring (acquire reuses stale live tokens too). \
                       Cancellation/timeout is not rollback; inspect state before retrying, especially \
                       append. Output is complete tagged YAML; refusals/failures set is_error."
    )]
    pub async fn drive_sheets_clear(
        &self,
        Parameters(params): Parameters<DriveSheetsClearParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = helpers::create_client_for(params.account.as_deref()).map_err(tool_error)?;
        run_sheets_clear(
            &client,
            &SheetsClient::from_drive_client(&client).map_err(tool_error)?,
            &params,
        )
        .await
        .map_err(tool_error)
    }

    /// Back up a Drive file and acquire a write lease.
    #[tool(
        description = "Back up a Drive file and acquire a write lease. Mirrors `omni-dev drive lease \
                       acquire`. Example: file_id:\"1a2B3c4D\", expiry_minutes:30. Prompts for device-owner \
                       authentication (Touch ID or account password; operator biometrics_only requires Touch \
                       ID). Native Docs/Sheets need the selected account's lease_backup_folder_id. \
                       Operator-only headless waiver applies; callers cannot set it. One consent covers \
                       repeated writes to that file until expiry. already-leased successfully returns the \
                       existing token, even if stale: release it via CLI or wait for expiry before renewing. \
                       The OS prompt can wait up to 120 seconds. Timeout/cancellation is not rollback: wait \
                       for the original call to finish, then explicitly repeat acquisition to recover an \
                       undelivered live token without another prompt/backup. Do not automatically race \
                       acquisitions. Output is tagged YAML; denied/unavailable/refused/failed set is_error."
    )]
    pub async fn drive_lease_acquire(
        &self,
        Parameters(params): Parameters<DriveLeaseAcquireParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = helpers::create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let opts = acquire_options(&params).map_err(tool_error)?;
        let authenticator = authenticate::platform_authenticator();
        run_lease_acquire(&client, &opts, authenticator.as_ref())
            .await
            .map_err(tool_error)
    }
}

fn yaml_outcome(outcome: &impl Serialize, is_error: bool) -> Result<CallToolResult> {
    let yaml = serde_yaml::to_string(outcome).context("Failed to serialize Drive outcome")?;
    Ok(if is_error {
        CallToolResult::error(vec![Content::text(yaml)])
    } else {
        CallToolResult::success(vec![Content::text(yaml)])
    })
}

fn docs_is_error(result: &docs_write::WriteResult) -> bool {
    use docs_write::WriteResult::{
        Appended, Blocked, Failed, RefusedLeaseExpired, RefusedLeaseStale, RefusedLeaseWrongFile,
        RefusedNoLease, RefusedNoRevisionId, RefusedNoVisibleParents, RefusedNotADocument,
        RefusedShortcut, Replaced, StaleRevision, WouldAppend, WouldReplace,
    };
    match result {
        WouldReplace { .. } | WouldAppend { .. } | Replaced { .. } | Appended { .. } => false,
        RefusedNotADocument { .. }
        | RefusedShortcut
        | RefusedNoVisibleParents
        | RefusedNoRevisionId
        | Blocked { .. }
        | RefusedNoLease
        | RefusedLeaseExpired
        | RefusedLeaseWrongFile
        | RefusedLeaseStale
        | StaleRevision { .. }
        | Failed { .. } => true,
    }
}

fn sheets_is_error(result: &sheets_write::WriteResult) -> bool {
    use sheets_write::WriteResult::{
        Appended, Blocked, Cleared, Failed, RefusedEmptyValues, RefusedLeaseExpired,
        RefusedLeaseStale, RefusedLeaseWrongFile, RefusedNoLease, RefusedNoVisibleParents,
        RefusedNotASpreadsheet, RefusedSheetNotFound, RefusedShortcut, WouldClear, WouldWrite,
        Written,
    };
    match result {
        WouldWrite { .. } | WouldClear | Written { .. } | Appended { .. } | Cleared { .. } => false,
        RefusedNotASpreadsheet { .. }
        | RefusedShortcut
        | RefusedNoVisibleParents
        | RefusedSheetNotFound { .. }
        | RefusedEmptyValues
        | Blocked { .. }
        | RefusedNoLease
        | RefusedLeaseExpired
        | RefusedLeaseWrongFile
        | RefusedLeaseStale
        | Failed { .. } => true,
    }
}

async fn run_docs_write(
    drive: &DriveClient,
    docs: &DocsClient,
    document_id: &str,
    payload: docs_write::WritePayload,
    dry_run: bool,
    lease: Option<String>,
    account: Option<&str>,
) -> Result<CallToolResult> {
    let opts = docs_write::WriteOptions {
        document_id: document_id.to_owned(),
        payload,
        dry_run,
        lease_token: lease,
        ledger_path: helpers::resolve_ledger_path(dry_run)?,
    };
    let rules = helpers::account_rules(account)?;
    let outcome = docs_write::write(drive, docs, &opts, &rules).await;
    yaml_outcome(&outcome, docs_is_error(&outcome.result))
}

fn append_text(params: &DriveDocsAppendParams) -> Result<String> {
    anyhow::ensure!(
        params.text.is_none() || params.text_path.is_none(),
        "Provide either text or text_path, not both"
    );
    if let Some(path) = &params.text_path {
        anyhow::ensure!(
            path != "-",
            "text_path must be a file; stdin is unsupported"
        );
        anyhow::ensure!(
            std::fs::metadata(path)?.len() <= crate::drive::files_api::MAX_UPLOAD_BYTES,
            "text_path exceeds the upload byte cap"
        );
    }
    let text = super::content_input::require_content_input(
        params.text.as_deref(),
        params.text_path.as_deref(),
        "text",
    )?;
    anyhow::ensure!(
        text.len() as u64 <= crate::drive::files_api::MAX_UPLOAD_BYTES,
        "text exceeds the upload byte cap"
    );
    Ok(text)
}

fn parse_values_format(value: Option<&str>) -> Result<ValuesFormat> {
    match value.unwrap_or("auto").to_ascii_lowercase().as_str() {
        "auto" => Ok(ValuesFormat::Auto),
        "csv" => Ok(ValuesFormat::Csv),
        "tsv" => Ok(ValuesFormat::Tsv),
        "json" => Ok(ValuesFormat::Json),
        _ => anyhow::bail!("values_format must be auto, csv, tsv or json"),
    }
}

fn parse_input(value: Option<&str>) -> Result<ValueInputOption> {
    match value
        .unwrap_or("user-entered")
        .to_ascii_lowercase()
        .as_str()
    {
        "user-entered" => Ok(ValueInputOption::UserEntered),
        "raw" => Ok(ValueInputOption::Raw),
        _ => anyhow::bail!("input must be user-entered or raw"),
    }
}

fn sheets_values(params: &DriveSheetsWriteParams) -> Result<Vec<Vec<String>>> {
    let format = parse_values_format(params.values_format.as_deref())?;
    match (&params.values, &params.values_path) {
        (Some(_), Some(_)) => anyhow::bail!("Provide either values or values_path, not both"),
        (Some(values), None) => {
            anyhow::ensure!(
                params.values_format.is_none(),
                "values_format applies only to values_path"
            );
            Ok(values.clone())
        }
        (None, Some(path)) => {
            anyhow::ensure!(
                path != "-",
                "values_path must be a file; stdin is unsupported"
            );
            crate::cli::drive::sheets::write::read_values(path, format)
        }
        (None, None) => anyhow::bail!("Provide either values or values_path"),
    }
}

async fn run_sheets_write(
    drive: &DriveClient,
    sheets: &SheetsClient,
    params: &DriveSheetsWriteParams,
    verb: sheets_write::WriteVerb,
) -> Result<CallToolResult> {
    let dry_run = params.dry_run.unwrap_or(false);
    let input = params.clone();
    let values = tokio::task::spawn_blocking(move || sheets_values(&input))
        .await
        .context("Values input task failed")??;
    let opts = sheets_write::WriteOptions {
        spreadsheet_id: params.spreadsheet_id.clone(),
        verb,
        range: params.range.clone(),
        sheet: params.sheet.clone(),
        values,
        input: parse_input(params.input.as_deref())?,
        dry_run,
        lease_token: params.lease.clone(),
        ledger_path: helpers::resolve_ledger_path(dry_run)?,
    };
    let rules = helpers::account_rules(params.account.as_deref())?;
    let outcome = sheets_write::write(drive, sheets, &opts, &rules).await;
    yaml_outcome(&outcome, sheets_is_error(&outcome.result))
}

async fn run_sheets_clear(
    drive: &DriveClient,
    sheets: &SheetsClient,
    params: &DriveSheetsClearParams,
) -> Result<CallToolResult> {
    let dry_run = params.dry_run.unwrap_or(false);
    let opts = sheets_write::WriteOptions {
        spreadsheet_id: params.spreadsheet_id.clone(),
        verb: sheets_write::WriteVerb::Clear,
        range: params.range.clone(),
        sheet: params.sheet.clone(),
        values: Vec::new(),
        input: ValueInputOption::default(),
        dry_run,
        lease_token: params.lease.clone(),
        ledger_path: helpers::resolve_ledger_path(dry_run)?,
    };
    let rules = helpers::account_rules(params.account.as_deref())?;
    let outcome = sheets_write::write(drive, sheets, &opts, &rules).await;
    yaml_outcome(&outcome, sheets_is_error(&outcome.result))
}

fn acquire_options(params: &DriveLeaseAcquireParams) -> Result<AcquireOptions> {
    let resolved = LeaseFlags {
        backup_dir: None,
        expiry_minutes: params.expiry_minutes,
        biometrics_only: false,
        allow_headless: false,
    }
    .resolve()?;
    Ok(AcquireOptions {
        file_id: params.file_id.clone(),
        backup_dir: resolved.backup_dir,
        expiry: resolved.expiry,
        auth_policy: resolved.auth_policy,
        allow_headless: resolved.allow_headless,
        native_backup_folder_id: helpers::account_lease_backup_folder_id(
            params.account.as_deref(),
        )?,
        ledger_path: crate::drive::lease::ledger::ledger_path()?,
        supersedes: None,
    })
}

async fn run_lease_acquire(
    drive: &DriveClient,
    opts: &AcquireOptions,
    authenticator: &dyn authenticate::Authenticator,
) -> Result<CallToolResult> {
    let outcome = acquire::acquire(drive, opts, authenticator).await;
    let is_error = match &outcome {
        AcquireResult::Acquired { .. } | AcquireResult::AlreadyLeased { .. } => false,
        AcquireResult::RefusedNativeDocument
        | AcquireResult::RefusedConcurrentChange { .. }
        | AcquireResult::Denied { .. }
        | AcquireResult::Unavailable { .. }
        | AcquireResult::Failed { .. } => true,
    };
    yaml_outcome(&outcome, is_error)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::lease::authenticate::AuthOutcome;
    use crate::drive::test_support::{
        client_with_bootstrapped_token, seed_lease, EnvGuard, FakeAuthenticator,
    };
    use crate::test_support::env::MapEnv;
    use crate::utils::settings::Settings;
    use serde_json::json;
    use wiremock::{
        matchers::{body_partial_json, method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    // EnvGuard serializes this HOME/settings-dependent test with other domains.
    // Restore logging overrides even if an assertion panics.
    struct LogRoute(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl LogRoute {
        fn new(path: &std::path::Path) -> Self {
            let snapshot = ["OMNI_DEV_LOG_FILE", "OMNI_DEV_LOG_DISABLE"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect();
            std::env::set_var("OMNI_DEV_LOG_FILE", path);
            std::env::remove_var("OMNI_DEV_LOG_DISABLE");
            Self(snapshot)
        }
    }
    impl Drop for LogRoute {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn decoded(result: &CallToolResult) -> serde_yaml::Value {
        let text = result.content[0].as_text().unwrap();
        serde_yaml::from_str(&text.text).unwrap()
    }

    fn params<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    fn configure(dir: &std::path::Path, allow: bool) {
        let rules = if allow {
            json!({"rules":[{"file_id":"target", "allow":["docs-write","sheets-write"], "require_lease":true}]})
        } else {
            json!({"rules":[{"file_id":"target", "deny":["docs-write","sheets-write"]}]})
        };
        Settings::upsert_drive_account(
            &dir.join(".omni-dev/settings.json"),
            "work",
            &[
                ("write_permissions", rules),
                ("lease_backup_folder_id", json!("backup-folder")),
            ],
        )
        .unwrap();
    }

    async fn clients(server: &MockServer) -> (DriveClient, DocsClient, SheetsClient) {
        let drive = client_with_bootstrapped_token(server).await;
        let env = MapEnv::new()
            .with(crate::drive::docs::client::DOCS_API_URL, &server.uri())
            .with(crate::drive::sheets::client::SHEETS_API_URL, &server.uri());
        let docs = DocsClient::from_drive_client_with(&env, &drive).unwrap();
        let sheets = SheetsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, docs, sheets)
    }

    async fn file(server: &MockServer, mime: &str) {
        Mock::given(method("GET")).and(path("/drive/v3/files/target"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"target", "name":"Target", "mimeType":mime, "version":"1", "parents":[]})))
            .mount(server).await;
    }

    async fn document(server: &MockServer) {
        Mock::given(method("GET")).and(path("/v1/documents/target"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"documentId":"target", "revisionId":"rev-1", "body":{"content":[{"startIndex":1,"endIndex":7,"paragraph":{"elements":[{"startIndex":1,"endIndex":7,"textRun":{"content":"draft\n"}}]}}]}})))
            .mount(server).await;
    }

    #[tokio::test]
    async fn all_five_verbs_preview_refuse_missing_lease_and_apply_a_valid_lease() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        configure(dir.path(), true);
        // Three stages for every verb. Exact mutation counts catch a preview/refusal
        // that accidentally writes, as well as a real write that does nothing.
        for verb in ["replace", "docs-append", "write", "sheets-append", "clear"] {
            let server = MockServer::start().await;
            let (drive, docs, sheets) = clients(&server).await;
            let is_docs = verb == "replace" || verb == "docs-append";
            file(
                &server,
                if is_docs {
                    crate::drive::types::GOOGLE_DOC_MIME_TYPE
                } else {
                    crate::drive::types::GOOGLE_SHEET_MIME_TYPE
                },
            )
            .await;
            if is_docs {
                document(&server).await;
                let request = if verb == "replace" {
                    json!({"requests":[{"replaceAllText":{"containsText":{"text":"draft", "matchCase":true},"replaceText":"final"}}],"writeControl":{"requiredRevisionId":"rev-1"}})
                } else {
                    json!({"writeControl":{"requiredRevisionId":"rev-1"}})
                };
                Mock::given(method("POST"))
                    .and(path("/v1/documents/target:batchUpdate"))
                    .and(body_partial_json(request))
                    .respond_with(ResponseTemplate::new(200).set_body_json(
                        json!({"replies":[{"replaceAllText":{"occurrencesChanged":1}}]}),
                    ))
                    .expect(1)
                    .mount(&server)
                    .await;
            } else {
                let endpoint = match verb {
                    "write" => "/v4/spreadsheets/target/values/A1:B2",
                    "sheets-append" => "/v4/spreadsheets/target/values/A1:B2:append",
                    _ => "/v4/spreadsheets/target/values/A1:B2:clear",
                };
                let mock = Mock::given(method(if verb == "write" { "PUT" } else { "POST" }))
                    .and(path(endpoint));
                let mock = if verb == "clear" {
                    mock
                } else {
                    mock.and(query_param("valueInputOption", "USER_ENTERED"))
                        .and(body_partial_json(json!({"values":[["a","b"]]})))
                };
                mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({"updatedRange":"A1:B2", "updatedCells":2, "clearedRange":"A1:B2", "updates":{"updatedRange":"A1:B2","updatedCells":2}}))).expect(1).mount(&server).await;
            }
            for stage in 0..3 {
                let token = (stage == 2).then(|| {
                    seed_lease(
                        &crate::drive::lease::ledger::ledger_path().unwrap(),
                        "target",
                        "1",
                    )
                });
                let result = if is_docs {
                    let payload = if verb == "replace" {
                        docs_write::WritePayload::Replace {
                            search: "draft".into(),
                            replace: "final".into(),
                            match_case: true,
                        }
                    } else {
                        docs_write::WritePayload::Append {
                            text: "new\n".into(),
                        }
                    };
                    run_docs_write(
                        &drive,
                        &docs,
                        "target",
                        payload,
                        stage == 0,
                        token,
                        Some("work"),
                    )
                    .await
                    .unwrap()
                } else if verb == "clear" {
                    run_sheets_clear(&drive, &sheets, &params(json!({"spreadsheet_id":"target", "range":"A1:B2", "dry_run":stage == 0,"lease":token,"account":"work"}))).await.unwrap()
                } else {
                    run_sheets_write(&drive, &sheets, &params(json!({"spreadsheet_id":"target","range":"A1:B2","values":[["a","b"]],"dry_run":stage == 0,"lease":token,"account":"work"})), if verb == "write" { sheets_write::WriteVerb::Write } else { sheets_write::WriteVerb::Append }).await.unwrap()
                };
                let yaml = decoded(&result);
                let expected = match (stage, verb) {
                    (0, "replace") => "would-replace",
                    (0, "docs-append") => "would-append",
                    (0, "clear") => "would-clear",
                    (0, _) => "would-write",
                    (1, _) => "refused-no-lease",
                    (2, "replace") => "replaced",
                    (2, "docs-append" | "sheets-append") => "appended",
                    (2, "write") => "written",
                    _ => "cleared",
                };
                assert_eq!(
                    yaml["result"]["status"].as_str(),
                    Some(expected),
                    "{verb}/{stage}: {yaml:?}"
                );
                assert_eq!(result.is_error, Some(stage == 1));
                if stage < 2 {
                    let requests = server.received_requests().await.unwrap();
                    assert!(!requests.iter().any(|r| r.method.as_str() == "PUT"
                        || r.url.path().ends_with(":batchUpdate")
                        || r.url.path().ends_with(":append")
                        || r.url.path().ends_with(":clear")));
                }
            }
        }
    }

    #[tokio::test]
    async fn blocked_writes_fetch_no_content_and_signal_error() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        configure(dir.path(), false);
        let log = dir.path().join("requests.jsonl");
        let _log_route = LogRoute::new(&log);
        let server = MockServer::start().await;
        let (drive, docs, _) = clients(&server).await;
        file(&server, crate::drive::types::GOOGLE_DOC_MIME_TYPE).await;
        let result = crate::request_log::CTX
            .scope(
                crate::request_log::RequestLogContext::mcp("drive_docs_append"),
                run_docs_write(
                    &drive,
                    &docs,
                    "target",
                    docs_write::WritePayload::Append {
                        text: "hello".into(),
                    },
                    false,
                    None,
                    Some("work"),
                ),
            )
            .await
            .unwrap();
        assert_eq!(decoded(&result)["result"]["status"], "blocked");
        assert_eq!(result.is_error, Some(true));
        let records: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mutation = records
            .iter()
            .find(|r| r["kind"] == "drivemutation")
            .unwrap();
        assert_eq!(mutation["source"], "mcp");
        assert_eq!(mutation["mcp_tool"], "drive_docs_append");
        assert_eq!(mutation["context"]["status"], "blocked");
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/token" || r.url.path() == "/drive/v3/files/target"));
    }

    #[tokio::test]
    async fn stale_lease_and_stale_revision_remain_distinct_errors() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        configure(dir.path(), true);
        let server = MockServer::start().await;
        let (drive, docs, _) = clients(&server).await;
        file(&server, crate::drive::types::GOOGLE_DOC_MIME_TYPE).await;
        document(&server).await;
        let ledger = crate::drive::lease::ledger::ledger_path().unwrap();
        let token = seed_lease(&ledger, "target", "0");
        let result = run_docs_write(
            &drive,
            &docs,
            "target",
            docs_write::WritePayload::Append {
                text: "hello".into(),
            },
            false,
            Some(token),
            Some("work"),
        )
        .await
        .unwrap();
        assert_eq!(decoded(&result)["result"]["status"], "refused-lease-stale");
        assert_eq!(result.is_error, Some(true));
        Mock::given(method("POST")).and(path("/v1/documents/target:batchUpdate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":{"code":400,"message":"The required revision ID 'rev-1' does not match the latest revision.","status":"INVALID_ARGUMENT"}}))).expect(1).mount(&server).await;
        let token = seed_lease(&ledger, "target", "1");
        let result = run_docs_write(
            &drive,
            &docs,
            "target",
            docs_write::WritePayload::Append {
                text: "hello".into(),
            },
            false,
            Some(token),
            Some("work"),
        )
        .await
        .unwrap();
        assert_eq!(decoded(&result)["result"]["status"], "stale-revision");
        assert_eq!(decoded(&result)["required_revision_id"], "rev-1");
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn inputs_validate_conflicts_formats_defaults_and_files() {
        assert_eq!(parse_input(None).unwrap(), ValueInputOption::UserEntered);
        assert_eq!(parse_input(Some("RAW")).unwrap(), ValueInputOption::Raw);
        assert!(parse_input(Some("invalid")).is_err());
        assert!(parse_values_format(Some("invalid")).is_err());
        let dir = tempfile::tempdir().unwrap();
        for (extension, content) in [
            ("csv", "a,b\n"),
            ("tsv", "a\tb\n"),
            ("json", r#"[["a","b"]]"#),
        ] {
            let p = dir.path().join(format!("cells.{extension}"));
            std::fs::write(&p, content).unwrap();
            for format in ["auto", extension] {
                let p: DriveSheetsWriteParams = params(
                    json!({"spreadsheet_id":"target","values_path":p,"values_format":format}),
                );
                assert_eq!(sheets_values(&p).unwrap(), vec![vec!["a", "b"]]);
            }
        }
        for value in [
            json!({}),
            json!({"values":[["a"]],"values_path":"missing"}),
            json!({"values_path":"-"}),
            json!({"values":[["a"]],"values_format":"csv"}),
        ] {
            let mut value = value;
            value["spreadsheet_id"] = json!("target");
            assert!(sheets_values(&params(value)).is_err());
        }
        for value in [
            json!({}),
            json!({"text":"a","text_path":"missing"}),
            json!({"text_path":"-"}),
        ] {
            let mut value = value;
            value["document_id"] = json!("target");
            assert!(append_text(&params(value)).is_err());
        }
        let text_path = dir.path().join("text.txt");
        std::fs::write(&text_path, "hello").unwrap();
        assert_eq!(
            append_text(&params(
                json!({"document_id":"target","text_path":text_path})
            ))
            .unwrap(),
            "hello"
        );
    }

    #[test]
    fn explicit_account_controls_rules_and_backup_folder() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        configure(dir.path(), true);
        Settings::upsert_drive_account(
            &dir.path().join(".omni-dev/settings.json"),
            "other",
            &[("lease_backup_folder_id", json!("other-backups"))],
        )
        .unwrap();
        std::env::set_var(crate::drive::account::DRIVE_ACCOUNT_ENV, "other");
        assert!(helpers::account_rules(None).unwrap().is_empty());
        assert_eq!(helpers::account_rules(Some("work")).unwrap().len(), 1);
        assert_eq!(
            helpers::account_lease_backup_folder_id(Some("work"))
                .unwrap()
                .as_deref(),
            Some("backup-folder")
        );
        assert_eq!(
            helpers::account_lease_backup_folder_id(None)
                .unwrap()
                .as_deref(),
            Some("other-backups")
        );
        assert!(helpers::account_rules(Some("unknown")).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acquisition_authorizes_native_backup_and_recovers_existing_token() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let server = MockServer::start().await;
        let drive = client_with_bootstrapped_token(&server).await;
        file(&server, crate::drive::types::GOOGLE_DOC_MIME_TYPE).await;
        Mock::given(method("POST")).and(path("/drive/v3/files/target/copy"))
            .and(body_partial_json(json!({"parents":["backup-folder"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"backup-copy","name":"Backup","mimeType":crate::drive::types::GOOGLE_DOC_MIME_TYPE}))).expect(1).mount(&server).await;
        let opts = AcquireOptions {
            file_id: "target".into(),
            backup_dir: dir.path().join("backups"),
            native_backup_folder_id: Some("backup-folder".into()),
            expiry: chrono::Duration::minutes(30),
            auth_policy: authenticate::AuthPolicy::DeviceOwner,
            ledger_path: dir.path().join("ledger.jsonl"),
            allow_headless: false,
            supersedes: None,
        };
        let result = run_lease_acquire(&drive, &opts, &FakeAuthenticator(AuthOutcome::Authorized))
            .await
            .unwrap();
        let yaml = decoded(&result);
        assert_eq!(yaml["status"], "acquired");
        assert_eq!(result.is_error, Some(false));
        let recovered = run_lease_acquire(
            &drive,
            &opts,
            &FakeAuthenticator(AuthOutcome::Denied("should not prompt".into())),
        )
        .await
        .unwrap();
        assert_eq!(decoded(&recovered)["status"], "already-leased");
        assert_eq!(decoded(&recovered)["token"], yaml["token"]);
        assert_eq!(recovered.is_error, Some(false));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acquisition_denied_and_unavailable_take_no_backup_and_mint_no_token() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let server = MockServer::start().await;
        let drive = client_with_bootstrapped_token(&server).await;
        file(&server, crate::drive::types::GOOGLE_DOC_MIME_TYPE).await;
        for (auth, status, waiver) in [
            (AuthOutcome::Denied("no".into()), "denied", false),
            (
                AuthOutcome::NoAuthenticator("headless".into()),
                "unavailable",
                false,
            ),
            (
                AuthOutcome::PolicyUnsatisfiable("no biometrics".into()),
                "unavailable",
                true,
            ),
        ] {
            let ledger = dir.path().join(format!("{status}-{waiver}.jsonl"));
            let opts = AcquireOptions {
                file_id: "target".into(),
                backup_dir: dir.path().join("backups"),
                native_backup_folder_id: Some("backup-folder".into()),
                expiry: chrono::Duration::minutes(30),
                auth_policy: authenticate::AuthPolicy::DeviceOwner,
                ledger_path: ledger.clone(),
                allow_headless: waiver,
                supersedes: None,
            };
            let result = run_lease_acquire(&drive, &opts, &FakeAuthenticator(auth))
                .await
                .unwrap();
            assert_eq!(decoded(&result)["status"], status);
            assert_eq!(result.is_error, Some(true));
            assert!(
                !ledger.exists() || std::fs::read_to_string(&ledger).unwrap().trim().is_empty()
            );
        }
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/token" || r.url.path() == "/drive/v3/files/target"));
    }

    #[tokio::test]
    async fn all_six_handlers_propagate_credentials_errors() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();
        let server = OmniDevServer::new();
        assert!(server
            .drive_docs_replace(Parameters(params(
                json!({"document_id":"target","search":"a","replace":"b"})
            )))
            .await
            .is_err());
        assert!(server
            .drive_docs_append(Parameters(params(
                json!({"document_id":"target","text":"a"})
            )))
            .await
            .is_err());
        assert!(server
            .drive_sheets_write(Parameters(params(
                json!({"spreadsheet_id":"target","values":[["a"]]})
            )))
            .await
            .is_err());
        assert!(server
            .drive_sheets_append(Parameters(params(
                json!({"spreadsheet_id":"target","values":[["a"]]})
            )))
            .await
            .is_err());
        assert!(server
            .drive_sheets_clear(Parameters(params(
                json!({"spreadsheet_id":"target","range":"A1"})
            )))
            .await
            .is_err());
        assert!(server
            .drive_lease_acquire(Parameters(params(json!({"file_id":"target"}))))
            .await
            .is_err());
    }
    #[test]
    fn mcp_sources_do_not_call_mutating_api_wrappers() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for (file, names) in [
            (
                "src/drive/files_api.rs",
                &[
                    "rename",
                    "move_to",
                    "trash",
                    "untrash",
                    "create",
                    "copy",
                    "upload",
                    "edit_content",
                ][..],
            ),
            ("src/drive/docs/api.rs", &["batch_update"][..]),
            (
                "src/drive/sheets/api.rs",
                &[
                    "values_update",
                    "values_append",
                    "values_clear",
                    "batch_update",
                    "copy_to",
                ][..],
            ),
        ] {
            let source = std::fs::read_to_string(manifest.join(file)).unwrap();
            for name in names {
                assert!(
                    source.contains(&format!("pub(in crate::drive) async fn {name}(")),
                    "{file}: mutation wrapper {name} must stay fenced"
                );
            }
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp");
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let production = source.split("#[cfg(test)]").next().unwrap();
            for method in [
                "batch_update",
                "values_update",
                "values_append",
                "values_clear",
                "edit_content",
                "rename",
                "move_to",
                "trash",
                "untrash",
                "copy_to",
                "copy",
                "upload",
                "create",
            ] {
                let direct = format!(".{method}(");
                let associated = format!("FilesApi::{method}(");
                // create is generic across services; check only the Drive-associated form.
                assert!(
                    (method == "create" || !production.contains(&direct))
                        && !production.contains(&associated),
                    "{} directly calls mutation wrapper {method}",
                    path.display()
                );
            }
        }
    }

    #[tokio::test]
    async fn a_write_ahead_audit_failure_is_an_error_without_mutation() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        configure(dir.path(), true);
        let server = MockServer::start().await;
        let (drive, docs, _) = clients(&server).await;
        file(&server, crate::drive::types::GOOGLE_DOC_MIME_TYPE).await;
        document(&server).await;
        let token = seed_lease(
            &crate::drive::lease::ledger::ledger_path().unwrap(),
            "target",
            "1",
        );
        let _audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        std::fs::create_dir(dir.path().join("audit.jsonl")).unwrap();
        let result = crate::request_log::CTX
            .scope(
                crate::request_log::RequestLogContext::mcp("drive_docs_append"),
                run_docs_write(
                    &drive,
                    &docs,
                    "target",
                    docs_write::WritePayload::Append {
                        text: "hello".into(),
                    },
                    false,
                    Some(token),
                    Some("work"),
                ),
            )
            .await
            .unwrap();
        assert_eq!(decoded(&result)["result"]["status"], "failed");
        assert_eq!(result.is_error, Some(true));
        assert!(!server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[test]
    fn expiry_is_bounded_and_acquisition_has_no_policy_override() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        configure(dir.path(), true);
        for expiry in [0, -1, 1441, i64::MAX] {
            assert!(acquire_options(&params(
                json!({"file_id":"target","expiry_minutes":expiry,"account":"work"})
            ))
            .is_err());
        }
        for expiry in [1, 30, 1440] {
            let opts = acquire_options(&params(
                json!({"file_id":"target","expiry_minutes":expiry,"account":"work"}),
            ))
            .unwrap();
            assert_eq!(opts.expiry, chrono::Duration::minutes(expiry));
            assert_eq!(
                opts.native_backup_folder_id.as_deref(),
                Some("backup-folder")
            );
            assert!(opts.supersedes.is_none());
        }
        assert!(serde_json::from_value::<DriveLeaseAcquireParams>(
            json!({"file_id":"target","allow_headless":true})
        )
        .is_err());
    }
    #[test]
    fn file_inputs_refuse_oversize_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(crate::drive::files_api::MAX_UPLOAD_BYTES + 1)
            .unwrap();
        assert!(
            append_text(&params(json!({"document_id":"target","text_path":path})))
                .unwrap_err()
                .to_string()
                .contains("cap")
        );
        assert!(sheets_values(&params(
            json!({"spreadsheet_id":"target","values_path":path})
        ))
        .unwrap_err()
        .to_string()
        .contains("cap"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn authentication_wait_does_not_stall_other_mcp_work() {
        struct WaitingAuthenticator {
            receive: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            started: std::sync::Arc<tokio::sync::Notify>,
        }
        impl authenticate::Authenticator for WaitingAuthenticator {
            fn authenticate(
                &self,
                _reason: &str,
                _policy: authenticate::AuthPolicy,
            ) -> AuthOutcome {
                self.started.notify_one();
                self.receive
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("runtime must keep other tasks alive during authentication");
                AuthOutcome::Denied("test consent declined".into())
            }
        }
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let server = MockServer::start().await;
        let drive = client_with_bootstrapped_token(&server).await;
        file(&server, crate::drive::types::GOOGLE_DOC_MIME_TYPE).await;
        let opts = AcquireOptions {
            file_id: "target".into(),
            backup_dir: dir.path().join("backups"),
            native_backup_folder_id: Some("backup-folder".into()),
            expiry: chrono::Duration::minutes(30),
            auth_policy: authenticate::AuthPolicy::DeviceOwner,
            ledger_path: dir.path().join("ledger.jsonl"),
            allow_headless: false,
            supersedes: None,
        };
        let (send, receive) = std::sync::mpsc::channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let heartbeat_started = started.clone();
        let acquire = tokio::spawn(async move {
            run_lease_acquire(
                &drive,
                &opts,
                &WaitingAuthenticator {
                    receive: std::sync::Mutex::new(receive),
                    started,
                },
            )
            .await
        });
        let heartbeat = tokio::spawn(async move {
            heartbeat_started.notified().await;
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            send.send(()).unwrap();
        });
        let result = acquire.await.unwrap().unwrap();
        heartbeat.await.unwrap();
        assert_eq!(decoded(&result)["status"], "denied");
    }
}
