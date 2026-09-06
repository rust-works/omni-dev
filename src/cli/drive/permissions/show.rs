//! CLI command for `omni-dev drive permissions show`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers::active_account_rules;
use crate::drive::write_gate::FolderPermissionRule;

/// Prints the active account's configured write-permission rules.
#[derive(Parser)]
pub struct ShowCommand {
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ShowCommand {
    /// Reads `write_permissions.rules` from `~/.omni-dev/settings.json`
    /// only — no network call, mirroring `drive account list`. Unlike that
    /// command, rules are per-account data (see
    /// `crate::drive::write_gate`'s module doc), so this resolves the
    /// active account first and shows nothing for an unconfigured account
    /// — there is no account whose `write_permissions` block could apply.
    pub fn execute(self) -> Result<()> {
        let rules = active_account_rules()?;
        run_show(&rules, &self.output)
    }
}

/// Emits `rules` in the requested format.
///
/// Split from [`ShowCommand::execute`] so tests can exercise rendering
/// directly against a constructed rule list, without touching `HOME`.
fn run_show(rules: &[FolderPermissionRule], output: &OutputFormat) -> Result<()> {
    if output_as(&rules.to_vec(), output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_rules_table(rules, &mut handle)
}

/// Renders rules as an aligned text table: `SCOPE | TARGET_ID | RECURSIVE
/// | ALLOW | DENY`. An empty input prints a message explaining that every
/// write is refused everywhere until a rule is configured.
///
/// `RECURSIVE` renders `-` for a file rule rather than `false`: a file has
/// no descendants, so `false` would imply the column means something there
/// (issue #1612).
fn render_rules_table(rules: &[FolderPermissionRule], out: &mut dyn Write) -> Result<()> {
    if rules.is_empty() {
        writeln!(
            out,
            "No write-permission rules configured for this account — every \
             create/upload/edit/sheets-write is refused everywhere. Add rules under \
             drive.accounts.<name>.write_permissions.rules in \
             ~/.omni-dev/settings.json, keyed on either a folder_id or a file_id."
        )
        .context("Failed to write empty-table message")?;
        return Ok(());
    }

    let scopes: Vec<&str> = rules.iter().map(rule_scope).collect();
    let scope_width = "SCOPE"
        .len()
        .max(scopes.iter().map(|s| s.len()).max().unwrap_or(0));
    let ids: Vec<String> = rules
        .iter()
        .map(|r| sanitize_for_terminal(rule_target_id(r)))
        .collect();
    let id_width = "TARGET_ID"
        .len()
        .max(ids.iter().map(String::len).max().unwrap_or(0));
    let recursives: Vec<String> = rules.iter().map(format_recursive).collect();
    let allow_strings: Vec<String> = rules.iter().map(|r| format_op_set(&r.allow)).collect();
    let allow_width = "ALLOW"
        .len()
        .max(allow_strings.iter().map(String::len).max().unwrap_or(0));
    let deny_strings: Vec<String> = rules.iter().map(|r| format_op_set(&r.deny)).collect();
    let deny_width = "DENY"
        .len()
        .max(deny_strings.iter().map(String::len).max().unwrap_or(0));

    writeln!(
        out,
        "{:<scope_width$}  {:<id_width$}  RECURSIVE  {:<allow_width$}  {:<deny_width$}",
        "SCOPE", "TARGET_ID", "ALLOW", "DENY"
    )
    .context("Failed to write header row")?;
    for i in 0..rules.len() {
        writeln!(
            out,
            "{:<scope_width$}  {:<id_width$}  {:<9}  {:<allow_width$}  {:<deny_width$}",
            scopes[i], ids[i], recursives[i], allow_strings[i], deny_strings[i],
        )
        .context("Failed to write rule row")?;
    }
    Ok(())
}

/// `"folder"` or `"file"` — which id this rule keys on.
///
/// Deserialization guarantees exactly one is set, so the fallback is
/// unreachable for any rule that came from a settings file.
fn rule_scope(rule: &FolderPermissionRule) -> &'static str {
    if rule.file_id.is_some() {
        "file"
    } else {
        "folder"
    }
}

/// The id this rule keys on, whichever kind it is.
fn rule_target_id(rule: &FolderPermissionRule) -> &str {
    rule.file_id
        .as_deref()
        .or(rule.folder_id.as_deref())
        .unwrap_or("-")
}

/// `true`/`false` for a folder rule, `-` for a file rule.
fn format_recursive(rule: &FolderPermissionRule) -> String {
    if rule.file_id.is_some() {
        "-".to_string()
    } else {
        rule.recursive.to_string()
    }
}

/// Renders an operation set as a stable, comma-joined, alphabetically
/// sorted string (`create,edit`), or `-` when empty.
fn format_op_set(
    ops: &std::collections::HashSet<crate::drive::write_gate::DriveOperation>,
) -> String {
    if ops.is_empty() {
        return "-".to_string();
    }
    let mut rendered: Vec<String> = ops.iter().map(ToString::to_string).collect();
    rendered.sort();
    rendered.join(",")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::drive::write_gate::DriveOperation;

    fn sample_rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: [DriveOperation::Create, DriveOperation::Upload]
                .into_iter()
                .collect(),
            deny: std::iter::once(DriveOperation::Edit).collect(),
        }
    }

    #[test]
    fn render_table_empty_explains_default_deny() {
        let mut buf = Vec::new();
        render_rules_table(&[], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("No write-permission rules configured"));
        assert!(out.contains("write_permissions"));
    }

    #[test]
    fn render_table_writes_header_and_rows() {
        let rules = [sample_rule()];
        let mut buf = Vec::new();
        render_rules_table(&rules, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("SCOPE"));
        assert!(out.contains("TARGET_ID"));
        assert!(out.contains("RECURSIVE"));
        assert!(out.contains("ALLOW"));
        assert!(out.contains("DENY"));
        assert!(out.contains("folder-1"));
        assert!(out.contains("true"));
        assert!(out.contains("create,upload"));
        assert!(out.contains("edit"));
    }

    #[test]
    fn render_table_uses_dash_for_empty_op_sets() {
        let rules = [FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: false,
            allow: std::collections::HashSet::default(),
            deny: std::collections::HashSet::default(),
        }];
        let mut buf = Vec::new();
        render_rules_table(&rules, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains('-'));
    }

    #[test]
    fn render_table_strips_control_bytes_from_folder_id() {
        let rules = [FolderPermissionRule {
            folder_id: Some("fo\rlder\x1b[31m".to_string()),
            file_id: None,
            recursive: false,
            allow: std::collections::HashSet::default(),
            deny: std::collections::HashSet::default(),
        }];
        let mut buf = Vec::new();
        render_rules_table(&rules, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
    }

    #[test]
    fn format_op_set_sorts_alphabetically_and_dedupes_via_hashset() {
        let ops: std::collections::HashSet<_> = [DriveOperation::Upload, DriveOperation::Create]
            .into_iter()
            .collect();
        assert_eq!(format_op_set(&ops), "create,upload");
    }

    #[test]
    fn run_show_table_path_writes_to_stdout() {
        run_show(&[sample_rule()], &OutputFormat::Table).unwrap();
    }

    #[test]
    fn run_show_json_path_returns_ok() {
        run_show(&[sample_rule()], &OutputFormat::Json).unwrap();
    }

    // ── file-id rules (issue #1612) ────────────────────────────────────

    fn sample_file_rule() -> FolderPermissionRule {
        FolderPermissionRule::file("sheet-1").allowing([DriveOperation::SheetsWrite])
    }

    #[test]
    fn render_table_labels_each_rule_with_its_scope() {
        let rules = [sample_rule(), sample_file_rule()];
        let mut buf = Vec::new();
        render_rules_table(&rules, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("SCOPE"), "{out}");
        assert!(out.contains("TARGET_ID"), "{out}");
        assert!(out.contains("folder"), "{out}");
        assert!(out.contains("file"), "{out}");
        assert!(out.contains("sheet-1"), "{out}");
    }

    #[test]
    fn render_table_shows_a_dash_not_false_for_a_file_rules_recursive() {
        // `false` would imply the column means something for a file rule.
        let rules = [sample_file_rule()];
        let mut buf = Vec::new();
        render_rules_table(&rules, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains("false"), "{out}");
    }

    #[test]
    fn render_table_strips_control_bytes_from_a_file_id_too() {
        let rules = [FolderPermissionRule::file("sh\reet\x1b[31m")];
        let mut buf = Vec::new();
        render_rules_table(&rules, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
    }

    #[test]
    fn the_empty_table_message_names_sheets_write_and_both_rule_keys() {
        let mut buf = Vec::new();
        render_rules_table(&[], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("sheets-write"), "{out}");
        assert!(out.contains("folder_id"), "{out}");
        assert!(out.contains("file_id"), "{out}");
    }
}
