//! CLI command for viewing JIRA issue changelogs.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::{JiraChangelogEntry, JiraChangelogItem};
use crate::cli::atlassian::helpers::create_client;

/// Shows change history for one or more JIRA issues.
#[derive(Parser)]
pub struct ChangelogCommand {
    /// Issue keys, comma-separated (e.g., PROJ-1 or PROJ-1,PROJ-2).
    pub keys: String,

    /// Maximum number of changelog entries per issue (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
}

impl ChangelogCommand {
    /// Fetches and displays changelogs for the specified issues.
    pub async fn execute(self) -> Result<()> {
        let keys = parse_keys(&self.keys);
        if keys.is_empty() {
            anyhow::bail!("No issue keys provided. Use --keys KEY1,KEY2,...");
        }

        let (client, _instance_url) = create_client()?;

        for (i, key) in keys.iter().enumerate() {
            if i > 0 {
                println!();
            }
            let entries = client.get_changelog(key, self.limit).await?;
            print_changelog(key, &entries);
        }

        Ok(())
    }
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

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn changelog_command_fields() {
        let cmd = ChangelogCommand {
            keys: "PROJ-1,PROJ-2".to_string(),
            limit: 50,
        };
        assert_eq!(cmd.keys, "PROJ-1,PROJ-2");
        assert_eq!(cmd.limit, 50);
    }
}
