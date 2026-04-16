//! CLI command for viewing JIRA issue changelogs.

use anyhow::Result;
use clap::Parser;

use serde::Serialize;

use crate::atlassian::client::{AtlassianClient, JiraChangelogEntry, JiraChangelogItem};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Shows change history for one or more JIRA issues.
#[derive(Parser)]
pub struct ChangelogCommand {
    /// Issue keys, comma-separated (e.g., PROJ-1 or PROJ-1,PROJ-2).
    pub keys: String,

    /// Maximum number of changelog entries per issue (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ChangelogCommand {
    /// Fetches and displays changelogs for the specified issues.
    pub async fn execute(self) -> Result<()> {
        let keys = parse_keys(&self.keys);
        if keys.is_empty() {
            anyhow::bail!("No issue keys provided. Use --keys KEY1,KEY2,...");
        }

        let (client, _instance_url) = create_client()?;
        run_changelog(&client, &keys, self.limit, &self.output).await
    }
}

/// Fetches and displays changelogs for the given issue keys.
async fn run_changelog(
    client: &AtlassianClient,
    keys: &[String],
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let mut all_changelogs: Vec<IssueChangelog> = Vec::new();
    for key in keys {
        let entries = client.get_changelog(key, limit).await?;
        all_changelogs.push(IssueChangelog {
            key: key.clone(),
            entries,
        });
    }

    if output_as(&all_changelogs, output)? {
        return Ok(());
    }

    for (i, changelog) in all_changelogs.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_changelog(&changelog.key, &changelog.entries);
    }

    Ok(())
}

/// Collected changelog for a single issue, used for json/yaml serialization.
#[derive(Serialize)]
struct IssueChangelog {
    key: String,
    entries: Vec<JiraChangelogEntry>,
}

/// Parses a comma-separated list of issue keys.
fn parse_keys(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Prints changelog entries for a single issue.
fn print_changelog(key: &str, entries: &[JiraChangelogEntry]) {
    if entries.is_empty() {
        println!("{key}: no changes.");
        return;
    }

    println!("{key}:");
    for entry in entries {
        let timestamp = format_timestamp(&entry.created);
        println!("  {timestamp} by {}", entry.author);
        for item in &entry.items {
            println!("    {} {}", item.field, format_change(item));
        }
    }
}

/// Formats a changelog item as "from → to".
fn format_change(item: &JiraChangelogItem) -> String {
    let from = item.from_string.as_deref().unwrap_or("(none)");
    let to = item.to_string.as_deref().unwrap_or("(none)");
    format!("{from} → {to}")
}

/// Formats an ISO 8601 timestamp to the date+time portion.
fn format_timestamp(ts: &str) -> &str {
    ts.split('.').next().unwrap_or(ts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_item(field: &str, from: Option<&str>, to: Option<&str>) -> JiraChangelogItem {
        JiraChangelogItem {
            field: field.to_string(),
            from_string: from.map(String::from),
            to_string: to.map(String::from),
        }
    }

    fn sample_entry(id: &str, author: &str, items: Vec<JiraChangelogItem>) -> JiraChangelogEntry {
        JiraChangelogEntry {
            id: id.to_string(),
            author: author.to_string(),
            created: "2026-04-01T10:00:00.000+0000".to_string(),
            items,
        }
    }

    // ── parse_keys ─────────────────────────────────────────────────

    #[test]
    fn parse_keys_basic() {
        assert_eq!(parse_keys("PROJ-1,PROJ-2"), vec!["PROJ-1", "PROJ-2"]);
    }

    #[test]
    fn parse_keys_with_whitespace() {
        assert_eq!(
            parse_keys("PROJ-1, PROJ-2 , PROJ-3"),
            vec!["PROJ-1", "PROJ-2", "PROJ-3"]
        );
    }

    #[test]
    fn parse_keys_single() {
        assert_eq!(parse_keys("PROJ-1"), vec!["PROJ-1"]);
    }

    #[test]
    fn parse_keys_empty() {
        assert!(parse_keys("").is_empty());
    }

    #[test]
    fn parse_keys_trailing_comma() {
        assert_eq!(parse_keys("PROJ-1,"), vec!["PROJ-1"]);
    }

    // ── format_change ──────────────────────────────────────────────

    #[test]
    fn format_change_both_values() {
        let item = sample_item("status", Some("Open"), Some("Done"));
        assert_eq!(format_change(&item), "Open → Done");
    }

    #[test]
    fn format_change_from_none() {
        let item = sample_item("assignee", None, Some("Alice"));
        assert_eq!(format_change(&item), "(none) → Alice");
    }

    #[test]
    fn format_change_to_none() {
        let item = sample_item("assignee", Some("Alice"), None);
        assert_eq!(format_change(&item), "Alice → (none)");
    }

    #[test]
    fn format_change_both_none() {
        let item = sample_item("field", None, None);
        assert_eq!(format_change(&item), "(none) → (none)");
    }

    // ── format_timestamp ───────────────────────────────────────────

    #[test]
    fn format_timestamp_with_millis() {
        assert_eq!(
            format_timestamp("2026-04-01T10:00:00.000+0000"),
            "2026-04-01T10:00:00"
        );
    }

    #[test]
    fn format_timestamp_without_millis() {
        assert_eq!(
            format_timestamp("2026-04-01T10:00:00"),
            "2026-04-01T10:00:00"
        );
    }

    #[test]
    fn format_timestamp_empty() {
        assert_eq!(format_timestamp(""), "");
    }

    // ── print_changelog ────────────────────────────────────────────

    #[test]
    fn print_changelog_empty() {
        print_changelog("PROJ-1", &[]);
    }

    #[test]
    fn print_changelog_with_entries() {
        let entries = vec![
            sample_entry(
                "100",
                "Alice",
                vec![
                    sample_item("status", Some("Open"), Some("In Progress")),
                    sample_item("assignee", None, Some("Bob")),
                ],
            ),
            sample_entry(
                "101",
                "Bob",
                vec![sample_item("priority", Some("Medium"), Some("High"))],
            ),
        ];
        print_changelog("PROJ-1", &entries);
    }

    #[test]
    fn print_changelog_no_items() {
        let entries = vec![sample_entry("100", "System", vec![])];
        print_changelog("PROJ-1", &entries);
    }

    // ── run_changelog (wiremock) ─────────────────────────────────────

    #[tokio::test]
    async fn run_changelog_single_key() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/changelog",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [{
                        "id": "100",
                        "author": {"displayName": "Alice"},
                        "created": "2026-04-01T10:00:00.000+0000",
                        "items": [{"field": "status", "fromString": "Open", "toString": "Done"}]
                    }],
                    "isLast": true
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let keys = vec!["PROJ-1".to_string()];
        assert!(run_changelog(&client, &keys, 50, &OutputFormat::Table)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_changelog_multiple_keys() {
        let server = wiremock::MockServer::start().await;
        for key in &["PROJ-1", "PROJ-2"] {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(format!(
                    "/rest/api/3/issue/{key}/changelog"
                )))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({
                        "values": [],
                        "isLast": true
                    }),
                ))
                .expect(1)
                .mount(&server)
                .await;
        }

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let keys = vec!["PROJ-1".to_string(), "PROJ-2".to_string()];
        assert!(run_changelog(&client, &keys, 50, &OutputFormat::Table)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_changelog_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/changelog",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let keys = vec!["NOPE-1".to_string()];
        let err = run_changelog(&client, &keys, 50, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn changelog_command_fields() {
        let cmd = ChangelogCommand {
            keys: "PROJ-1,PROJ-2".to_string(),
            limit: 50,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.keys, "PROJ-1,PROJ-2");
        assert_eq!(cmd.limit, 50);
    }
}
