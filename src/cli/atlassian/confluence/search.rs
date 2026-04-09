//! CLI command for searching Confluence pages with CQL.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::ConfluenceSearchResults;
use crate::cli::atlassian::helpers::create_client;

/// Searches Confluence pages using CQL.
#[derive(Parser)]
pub struct SearchCommand {
    /// Raw CQL query string (e.g., "space = ENG AND title ~ 'architecture'").
    #[arg(long)]
    pub cql: Option<String>,

    /// Filter by space key.
    #[arg(long)]
    pub space: Option<String>,

    /// Filter by title (substring match).
    #[arg(long)]
    pub title: Option<String>,

    /// Maximum number of results (default: 25).
    #[arg(long, default_value_t = 25)]
    pub max_results: u32,
}

impl SearchCommand {
    /// Executes the search and prints results as a table.
    pub async fn execute(self) -> Result<()> {
        let cql = self.build_cql()?;
        let (client, _instance_url) = create_client()?;

        let result = client.search_confluence(&cql, self.max_results).await?;
        print_search_results(&result);

        Ok(())
    }

    /// Builds a CQL query from the provided flags, or returns the raw `--cql` value.
    fn build_cql(&self) -> Result<String> {
        if let Some(ref cql) = self.cql {
            return Ok(cql.clone());
        }

        let mut clauses = vec!["type = \"page\"".to_string()];

        if let Some(ref space) = self.space {
            clauses.push(format!("space = \"{space}\""));
        }
        if let Some(ref title) = self.title {
            clauses.push(format!("title ~ \"{title}\""));
        }

        if clauses.len() == 1 && self.space.is_none() && self.title.is_none() {
            anyhow::bail!(
                "Provide --cql for a raw query, or at least one filter flag (--space, --title)"
            );
        }

        Ok(clauses.join(" AND "))
    }
}

/// Prints search results as a formatted table.
fn print_search_results(result: &ConfluenceSearchResults) {
    if result.results.is_empty() {
        println!("No pages found.");
        return;
    }

    // Calculate column widths
    let id_width = result
        .results
        .iter()
        .map(|r| r.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let space_width = result
        .results
        .iter()
        .map(|r| r.space_key.len())
        .max()
        .unwrap_or(5)
        .max(5);

    // Header
    let title_sep = "-".repeat(5);
    println!("{:<id_width$}  {:<space_width$}  TITLE", "ID", "SPACE");
    println!(
        "{:<id_width$}  {:<space_width$}  {title_sep}",
        "-".repeat(id_width),
        "-".repeat(space_width),
    );

    // Rows
    for page in &result.results {
        println!(
            "{:<id_width$}  {:<space_width$}  {}",
            page.id, page.space_key, page.title
        );
    }

    // Pagination note
    if result.total > result.results.len() as u32 {
        println!(
            "\nShowing {} of {} results.",
            result.results.len(),
            result.total
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::ConfluenceSearchResult;

    fn sample_page(id: &str, title: &str, space: &str) -> ConfluenceSearchResult {
        ConfluenceSearchResult {
            id: id.to_string(),
            title: title.to_string(),
            space_key: space.to_string(),
        }
    }

    // ── build_cql ──────────────────────────────────────────────────

    #[test]
    fn build_cql_from_raw() {
        let cmd = SearchCommand {
            cql: Some("space = ENG ORDER BY title".to_string()),
            space: None,
            title: None,
            max_results: 25,
        };
        assert_eq!(cmd.build_cql().unwrap(), "space = ENG ORDER BY title");
    }

    #[test]
    fn build_cql_from_space() {
        let cmd = SearchCommand {
            cql: None,
            space: Some("ENG".to_string()),
            title: None,
            max_results: 25,
        };
        let cql = cmd.build_cql().unwrap();
        assert!(cql.contains("type = \"page\""));
        assert!(cql.contains("space = \"ENG\""));
    }

    #[test]
    fn build_cql_from_title() {
        let cmd = SearchCommand {
            cql: None,
            space: None,
            title: Some("architecture".to_string()),
            max_results: 25,
        };
        let cql = cmd.build_cql().unwrap();
        assert!(cql.contains("title ~ \"architecture\""));
    }

    #[test]
    fn build_cql_from_space_and_title() {
        let cmd = SearchCommand {
            cql: None,
            space: Some("ENG".to_string()),
            title: Some("auth".to_string()),
            max_results: 10,
        };
        let cql = cmd.build_cql().unwrap();
        assert!(cql.contains("type = \"page\""));
        assert!(cql.contains("space = \"ENG\""));
        assert!(cql.contains("title ~ \"auth\""));
        assert!(cql.contains(" AND "));
    }

    #[test]
    fn build_cql_no_flags_errors() {
        let cmd = SearchCommand {
            cql: None,
            space: None,
            title: None,
            max_results: 25,
        };
        assert!(cmd.build_cql().is_err());
    }

    #[test]
    fn build_cql_raw_overrides_flags() {
        let cmd = SearchCommand {
            cql: Some("title = \"override\"".to_string()),
            space: Some("ENG".to_string()),
            title: Some("ignored".to_string()),
            max_results: 25,
        };
        assert_eq!(cmd.build_cql().unwrap(), "title = \"override\"");
    }

    // ── print_search_results ───────────────────────────────────────

    #[test]
    fn print_results_empty() {
        let result = ConfluenceSearchResults {
            results: vec![],
            total: 0,
        };
        print_search_results(&result);
    }

    #[test]
    fn print_results_with_pages() {
        let result = ConfluenceSearchResults {
            results: vec![
                sample_page("12345", "Architecture Overview", "ENG"),
                sample_page("67890", "Getting Started", "DOC"),
            ],
            total: 2,
        };
        print_search_results(&result);
    }

    #[test]
    fn print_results_with_pagination() {
        let result = ConfluenceSearchResults {
            results: vec![sample_page("111", "First Page", "ENG")],
            total: 50,
        };
        print_search_results(&result);
    }

    #[test]
    fn print_results_empty_space_key() {
        let result = ConfluenceSearchResults {
            results: vec![sample_page("999", "Orphan Page", "")],
            total: 1,
        };
        print_search_results(&result);
    }

    // ── SearchCommand struct ───────────────────────────────────────

    #[test]
    fn search_command_defaults() {
        let cmd = SearchCommand {
            cql: None,
            space: None,
            title: None,
            max_results: 25,
        };
        assert!(cmd.cql.is_none());
        assert_eq!(cmd.max_results, 25);
    }
}
