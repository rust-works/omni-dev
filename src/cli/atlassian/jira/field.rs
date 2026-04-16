//! CLI commands for JIRA field metadata.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AtlassianClient, JiraField, JiraFieldOption};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA field definitions and options.
#[derive(Parser)]
pub struct FieldCommand {
    /// The field subcommand to execute.
    #[command(subcommand)]
    pub command: FieldSubcommands,
}

/// Field subcommands.
#[derive(Subcommand)]
pub enum FieldSubcommands {
    /// Lists all field definitions.
    List(ListCommand),
    /// Shows options for a custom field.
    Options(OptionsCommand),
}

impl FieldCommand {
    /// Executes the field command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            FieldSubcommands::List(cmd) => cmd.execute().await,
            FieldSubcommands::Options(cmd) => cmd.execute().await,
        }
    }
}

/// Lists all field definitions.
#[derive(Parser)]
pub struct ListCommand {
    /// Filter fields by name (case-insensitive substring match).
    #[arg(long)]
    pub search: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays field definitions.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list_fields(&client, self.search.as_deref(), &self.output).await
    }
}

/// Shows options for a custom field.
#[derive(Parser)]
pub struct OptionsCommand {
    /// Field ID (e.g., "customfield_10001").
    #[arg(long)]
    pub field_id: String,

    /// Context ID (auto-discovered if omitted).
    #[arg(long)]
    pub context_id: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl OptionsCommand {
    /// Fetches and displays field options.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_field_options(
            &client,
            &self.field_id,
            self.context_id.as_deref(),
            &self.output,
        )
        .await
    }
}

/// Fetches, filters, and displays field definitions.
async fn run_list_fields(
    client: &AtlassianClient,
    search: Option<&str>,
    output: &OutputFormat,
) -> Result<()> {
    let fields = client.get_fields().await?;
    let filtered = filter_fields(&fields, search);
    if output_as(&filtered, output)? {
        return Ok(());
    }
    print_fields(&filtered);
    Ok(())
}

/// Fetches and displays options for a custom field.
async fn run_field_options(
    client: &AtlassianClient,
    field_id: &str,
    context_id: Option<&str>,
    output: &OutputFormat,
) -> Result<()> {
    let options = client.get_field_options(field_id, context_id).await?;
    if output_as(&options, output)? {
        return Ok(());
    }
    print_options(&options);
    Ok(())
}

/// Filters fields by a case-insensitive substring match on the name.
fn filter_fields<'a>(fields: &'a [JiraField], search: Option<&str>) -> Vec<&'a JiraField> {
    match search {
        Some(query) => {
            let query_lower = query.to_lowercase();
            fields
                .iter()
                .filter(|f| f.name.to_lowercase().contains(&query_lower))
                .collect()
        }
        None => fields.iter().collect(),
    }
}

/// Prints fields as a formatted table.
fn print_fields(fields: &[&JiraField]) {
    if fields.is_empty() {
        println!("No fields found.");
        return;
    }

    let id_width = fields.iter().map(|f| f.id.len()).max().unwrap_or(2).max(2);
    let type_width = fields
        .iter()
        .filter_map(|f| f.schema_type.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);

    println!(
        "{:<id_width$}  {:<6}  {:<type_width$}  NAME",
        "ID", "CUSTOM", "TYPE"
    );
    let name_sep = "-".repeat(4);
    println!(
        "{:<id_width$}  {:<6}  {:<type_width$}  {name_sep}",
        "-".repeat(id_width),
        "-".repeat(6),
        "-".repeat(type_width),
    );

    for field in fields {
        let custom = if field.custom { "yes" } else { "no" };
        let schema = field.schema_type.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<6}  {:<type_width$}  {}",
            field.id, custom, schema, field.name
        );
    }
}

/// Prints field options as a formatted table.
fn print_options(options: &[JiraFieldOption]) {
    if options.is_empty() {
        println!("No options found.");
        return;
    }

    let id_width = options.iter().map(|o| o.id.len()).max().unwrap_or(2).max(2);

    println!("{:<id_width$}  VALUE", "ID");
    let val_sep = "-".repeat(5);
    println!("{:<id_width$}  {val_sep}", "-".repeat(id_width));

    for option in options {
        println!("{:<id_width$}  {}", option.id, option.value);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_field(id: &str, name: &str, custom: bool, schema_type: Option<&str>) -> JiraField {
        JiraField {
            id: id.to_string(),
            name: name.to_string(),
            custom,
            schema_type: schema_type.map(String::from),
        }
    }

    fn sample_option(id: &str, value: &str) -> JiraFieldOption {
        JiraFieldOption {
            id: id.to_string(),
            value: value.to_string(),
        }
    }

    // ── filter_fields ──────────────────────────────────────────────

    #[test]
    fn filter_no_query_returns_all() {
        let fields = vec![
            sample_field("summary", "Summary", false, Some("string")),
            sample_field("customfield_10001", "Story Points", true, Some("number")),
        ];
        let result = filter_fields(&fields, None);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_by_name_case_insensitive() {
        let fields = vec![
            sample_field("summary", "Summary", false, Some("string")),
            sample_field("customfield_10001", "Story Points", true, Some("number")),
            sample_field("status", "Status", false, Some("status")),
        ];
        let result = filter_fields(&fields, Some("story"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "customfield_10001");
    }

    #[test]
    fn filter_no_match() {
        let fields = vec![sample_field("summary", "Summary", false, None)];
        let result = filter_fields(&fields, Some("nonexistent"));
        assert!(result.is_empty());
    }

    // ── print_fields ───────────────────────────────────────────────

    #[test]
    fn print_fields_empty() {
        print_fields(&[]);
    }

    #[test]
    fn print_fields_with_data() {
        let fields = vec![
            sample_field("summary", "Summary", false, Some("string")),
            sample_field("customfield_10001", "Story Points", true, Some("number")),
        ];
        let refs: Vec<&JiraField> = fields.iter().collect();
        print_fields(&refs);
    }

    #[test]
    fn print_fields_no_schema() {
        let fields = vec![sample_field("labels", "Labels", false, None)];
        let refs: Vec<&JiraField> = fields.iter().collect();
        print_fields(&refs);
    }

    // ── print_options ──────────────────────────────────────────────

    #[test]
    fn print_options_empty() {
        print_options(&[]);
    }

    #[test]
    fn print_options_with_data() {
        let options = vec![
            sample_option("1", "High"),
            sample_option("2", "Medium"),
            sample_option("3", "Low"),
        ];
        print_options(&options);
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn field_command_list_variant() {
        let cmd = FieldCommand {
            command: FieldSubcommands::List(ListCommand {
                search: None,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, FieldSubcommands::List(_)));
    }

    #[test]
    fn field_command_options_variant() {
        let cmd = FieldCommand {
            command: FieldSubcommands::Options(OptionsCommand {
                field_id: "customfield_10001".to_string(),
                context_id: None,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, FieldSubcommands::Options(_)));
    }

    #[test]
    fn list_command_with_search() {
        let cmd = ListCommand {
            search: Some("story".to_string()),
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.search.as_deref(), Some("story"));
    }

    #[test]
    fn options_command_with_context() {
        let cmd = OptionsCommand {
            field_id: "customfield_10001".to_string(),
            context_id: Some("12345".to_string()),
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.context_id.as_deref(), Some("12345"));
    }
}
