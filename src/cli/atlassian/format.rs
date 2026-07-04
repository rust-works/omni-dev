//! Atlassian-specific output-format helpers.
//!
//! The machine-readable [`OutputFormat`] enum and the generic rendering
//! machinery (`write_output`/`output_as`, the [`JsonlSerialize`] trait, and the
//! blanket `Vec<T>` impl) live in the shared [`crate::cli::format`] module and
//! are re-exported here so the Atlassian command call sites are unchanged. This
//! module adds only the [`ContentFormat`] input/output enum (JFM vs ADF) and the
//! `JsonlSerialize` impls for Atlassian collection wrapper types.

use std::io::Write;

use anyhow::Result;
use clap::ValueEnum;

pub use crate::cli::format::{
    output_as, write_items_jsonl, write_scalar_jsonl, JsonlSerialize, OutputFormat,
};

use crate::atlassian::confluence_types::{
    ConfluenceAttachmentPage, ConfluenceSpacePage, PageSummaryPage,
};
use crate::atlassian::confluence_types::{
    ConfluenceSearchResults, ConfluenceUserGetResults, ConfluenceUserSearchResults,
};
use crate::atlassian::jira_types::{
    AgileBoardList, AgileSprintList, CreateMeta, JiraDevStatus, JiraDevStatusSummary,
    JiraProjectList, JiraProjectVersionList, JiraSearchResult, JiraUserGetResults,
    JiraUserSearchResults, JiraWatcherList, JiraWorklogList,
};

/// Output/input format for Atlassian content (read/write/create commands).
#[derive(Clone, Debug, Default, ValueEnum)]
pub enum ContentFormat {
    /// JFM markdown with YAML frontmatter.
    #[default]
    Jfm,
    /// Raw Atlassian Document Format JSON.
    Adf,
}

impl JsonlSerialize for AgileBoardList {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.boards.iter(), out)
    }
}

impl JsonlSerialize for AgileSprintList {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.sprints.iter(), out)
    }
}

impl JsonlSerialize for JiraSearchResult {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.issues.iter(), out)
    }
}

impl JsonlSerialize for JiraProjectList {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.projects.iter(), out)
    }
}

impl JsonlSerialize for CreateMeta {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.fields.iter(), out)
    }
}

impl JsonlSerialize for JiraProjectVersionList {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.versions.iter(), out)
    }
}

impl JsonlSerialize for JiraWatcherList {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.watchers.iter(), out)
    }
}

impl JsonlSerialize for JiraWorklogList {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.worklogs.iter(), out)
    }
}

impl JsonlSerialize for ConfluenceSearchResults {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.results.iter(), out)
    }
}

impl JsonlSerialize for ConfluenceUserSearchResults {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.users.iter(), out)
    }
}

impl JsonlSerialize for ConfluenceAttachmentPage {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.results.iter(), out)
    }
}

impl JsonlSerialize for ConfluenceSpacePage {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.results.iter(), out)
    }
}

impl JsonlSerialize for PageSummaryPage {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.results.iter(), out)
    }
}

impl JsonlSerialize for JiraUserSearchResults {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.users.iter(), out)
    }
}

impl JsonlSerialize for JiraUserGetResults {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.users.iter(), out)
    }
}

impl JsonlSerialize for ConfluenceUserGetResults {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.users.iter(), out)
    }
}

impl JsonlSerialize for JiraDevStatus {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

impl JsonlSerialize for JiraDevStatusSummary {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::atlassian::confluence_types::{ConfluenceSearchResult, ConfluenceUserSearchResult};
    use crate::atlassian::jira_types::{
        AgileBoard, AgileSprint, JiraComment, JiraDevStatusCount, JiraIssue, JiraProject,
        JiraProjectVersion, JiraUser, JiraWorklog,
    };
    use crate::cli::format::write_output;

    // ── ContentFormat ──────────────────────────────────────────────

    #[test]
    fn default_is_jfm() {
        let format = ContentFormat::default();
        assert!(matches!(format, ContentFormat::Jfm));
    }

    #[test]
    fn jfm_variant() {
        let format = ContentFormat::Jfm;
        assert!(matches!(format, ContentFormat::Jfm));
    }

    #[test]
    fn adf_variant() {
        let format = ContentFormat::Adf;
        assert!(matches!(format, ContentFormat::Adf));
    }

    #[test]
    fn debug_format() {
        assert_eq!(format!("{:?}", ContentFormat::Jfm), "Jfm");
        assert_eq!(format!("{:?}", ContentFormat::Adf), "Adf");
    }

    #[test]
    fn clone() {
        let format = ContentFormat::Adf;
        let cloned = format;
        assert!(matches!(cloned, ContentFormat::Adf));
    }

    // ── sample data helpers ────────────────────────────────────────

    fn sample_board(id: u64, name: &str) -> AgileBoard {
        AgileBoard {
            id,
            name: name.to_string(),
            board_type: "scrum".to_string(),
            project_key: None,
        }
    }

    fn sample_sprint(id: u64, name: &str) -> AgileSprint {
        AgileSprint {
            id,
            name: name.to_string(),
            state: "active".to_string(),
            goal: None,
            start_date: None,
            end_date: None,
        }
    }

    fn sample_issue(key: &str) -> JiraIssue {
        JiraIssue {
            key: key.to_string(),
            summary: "s".to_string(),
            description_adf: None,
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: vec![],
        }
    }

    fn sample_project(key: &str) -> JiraProject {
        JiraProject {
            key: key.to_string(),
            id: "1".to_string(),
            name: "Project".to_string(),
            project_type: None,
            lead: None,
        }
    }

    fn sample_project_version(id: &str, name: &str) -> JiraProjectVersion {
        JiraProjectVersion {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            project_key: "PROJ".to_string(),
            released: false,
            archived: false,
            release_date: None,
            start_date: None,
        }
    }

    fn sample_user(name: &str) -> JiraUser {
        JiraUser {
            account_id: name.to_string(),
            display_name: name.to_string(),
            email_address: None,
        }
    }

    fn sample_worklog(id: &str) -> JiraWorklog {
        JiraWorklog {
            id: id.to_string(),
            author: "alice".to_string(),
            time_spent: "1h".to_string(),
            time_spent_seconds: 3600,
            started: "2025-01-01T00:00:00Z".to_string(),
            comment: None,
        }
    }

    fn sample_confluence_result(id: &str) -> ConfluenceSearchResult {
        ConfluenceSearchResult {
            id: id.to_string(),
            title: "Title".to_string(),
            space_key: "ENG".to_string(),
        }
    }

    fn sample_confluence_user(id: &str) -> ConfluenceUserSearchResult {
        ConfluenceUserSearchResult {
            account_id: Some(id.to_string()),
            display_name: "Name".to_string(),
            email: None,
        }
    }

    fn sample_comment(id: &str, _body: &str) -> JiraComment {
        JiraComment {
            id: id.to_string(),
            author: "alice".to_string(),
            created: "2025-01-01T00:00:00Z".to_string(),
            body_adf: None,
            updated: None,
        }
    }

    // ── wrapper type JsonlSerialize impls ──────────────────────────

    fn jsonl_string<T: JsonlSerialize>(value: &T) -> String {
        let mut buf = Vec::new();
        value.write_jsonl(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn vec_of_refs_jsonl() {
        let comment = sample_comment("c1", "hello");
        let comments = vec![&comment];
        let mut buf = Vec::new();
        comments.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with('{'));
        assert!(out.ends_with('\n'));
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn agile_board_list_jsonl() {
        let list = AgileBoardList {
            boards: vec![sample_board(1, "B1"), sample_board(2, "B2")],
            total: 2,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"id\":1"));
        assert!(out.contains("\"id\":2"));
    }

    #[test]
    fn agile_board_list_jsonl_empty() {
        let list = AgileBoardList {
            boards: vec![],
            total: 0,
        };
        assert_eq!(jsonl_string(&list), "");
    }

    #[test]
    fn agile_sprint_list_jsonl() {
        let list = AgileSprintList {
            sprints: vec![sample_sprint(1, "Sprint 1")],
            total: 1,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"id\":1"));
    }

    #[test]
    fn jira_search_result_jsonl() {
        let result = JiraSearchResult {
            issues: vec![sample_issue("A-1"), sample_issue("A-2")],
            total: 2,
        };
        let out = jsonl_string(&result);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"key\":\"A-1\""));
        assert!(out.contains("\"key\":\"A-2\""));
    }

    #[test]
    fn jira_project_list_jsonl() {
        let list = JiraProjectList {
            projects: vec![sample_project("P")],
            total: 1,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"key\":\"P\""));
    }

    #[test]
    fn jira_project_version_list_jsonl() {
        let list = JiraProjectVersionList {
            versions: vec![
                sample_project_version("10", "1.0.0"),
                sample_project_version("11", "1.1.0"),
            ],
            total: 2,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"id\":\"10\""));
        assert!(out.contains("\"id\":\"11\""));
    }

    #[test]
    fn jira_watcher_list_jsonl() {
        let list = JiraWatcherList {
            watchers: vec![sample_user("alice"), sample_user("bob")],
            watch_count: 2,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn jira_worklog_list_jsonl() {
        let list = JiraWorklogList {
            worklogs: vec![sample_worklog("w1")],
            total: 1,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"id\":\"w1\""));
    }

    #[test]
    fn confluence_search_results_jsonl() {
        let list = ConfluenceSearchResults {
            results: vec![sample_confluence_result("r1")],
            total: 1,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"id\":\"r1\""));
    }

    #[test]
    fn confluence_user_search_results_jsonl() {
        let list = ConfluenceUserSearchResults {
            users: vec![sample_confluence_user("u1")],
            total: 1,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"accountId\":\"u1\"") || out.contains("\"account_id\":\"u1\""));
    }

    #[test]
    fn confluence_attachment_page_jsonl() {
        use crate::atlassian::confluence_types::{ConfluenceAttachment, ConfluenceAttachmentPage};
        let page = ConfluenceAttachmentPage {
            results: vec![
                ConfluenceAttachment {
                    id: "a1".to_string(),
                    title: "one.png".to_string(),
                    media_type: Some("image/png".to_string()),
                    file_size: Some(100),
                    download_url: None,
                    version: None,
                    page_id: None,
                    file_id: None,
                },
                ConfluenceAttachment {
                    id: "a2".to_string(),
                    title: "two.pdf".to_string(),
                    media_type: None,
                    file_size: None,
                    download_url: None,
                    version: None,
                    page_id: None,
                    file_id: None,
                },
            ],
            next_cursor: Some("NEXT".to_string()),
        };
        let out = jsonl_string(&page);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"id\":\"a1\""));
        assert!(out.contains("\"id\":\"a2\""));
    }

    #[test]
    fn confluence_attachment_page_jsonl_empty() {
        use crate::atlassian::confluence_types::ConfluenceAttachmentPage;
        let page = ConfluenceAttachmentPage {
            results: vec![],
            next_cursor: None,
        };
        assert_eq!(jsonl_string(&page), "");
    }

    #[test]
    fn confluence_space_page_jsonl() {
        use crate::atlassian::confluence_types::{ConfluenceSpace, ConfluenceSpacePage};
        let page = ConfluenceSpacePage {
            results: vec![
                ConfluenceSpace {
                    id: "100".to_string(),
                    key: "ENG".to_string(),
                    name: "Engineering".to_string(),
                    type_: "global".to_string(),
                    status: "current".to_string(),
                    homepage_id: Some("200".to_string()),
                },
                ConfluenceSpace {
                    id: "101".to_string(),
                    key: "OPS".to_string(),
                    name: "Operations".to_string(),
                    type_: "global".to_string(),
                    status: "archived".to_string(),
                    homepage_id: None,
                },
            ],
            next_cursor: Some("NEXT".to_string()),
        };
        let out = jsonl_string(&page);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"id\":\"100\""));
        assert!(out.contains("\"key\":\"ENG\""));
        assert!(out.contains("\"homepageId\":\"200\""));
    }

    #[test]
    fn confluence_space_page_jsonl_empty() {
        use crate::atlassian::confluence_types::ConfluenceSpacePage;
        let page = ConfluenceSpacePage {
            results: vec![],
            next_cursor: None,
        };
        assert_eq!(jsonl_string(&page), "");
    }

    #[test]
    fn jira_user_search_results_jsonl() {
        use crate::atlassian::jira_types::{JiraUserSearchResult, JiraUserSearchResults};
        let list = JiraUserSearchResults {
            users: vec![
                JiraUserSearchResult {
                    account_id: "u1".to_string(),
                    display_name: Some("Alice".to_string()),
                    email_address: Some("alice@example.com".to_string()),
                    active: true,
                    account_type: Some("atlassian".to_string()),
                },
                JiraUserSearchResult {
                    account_id: "u2".to_string(),
                    display_name: None,
                    email_address: None,
                    active: false,
                    account_type: None,
                },
            ],
            count: 2,
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"account_id\":\"u1\""));
        assert!(out.contains("\"account_id\":\"u2\""));
    }

    #[test]
    fn jira_user_get_results_jsonl() {
        use crate::atlassian::jira_types::{JiraUserGetResults, JiraUserRecord};
        let list = JiraUserGetResults {
            users: vec![
                JiraUserRecord {
                    account_id: "u1".to_string(),
                    display_name: Some("Alice".to_string()),
                    email_address: Some("alice@example.com".to_string()),
                    active: Some(true),
                    account_type: Some("atlassian".to_string()),
                    error: None,
                },
                JiraUserRecord {
                    account_id: "bad".to_string(),
                    display_name: None,
                    email_address: None,
                    active: None,
                    account_type: None,
                    error: Some("HTTP 404".to_string()),
                },
            ],
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"account_id\":\"u1\""));
        assert!(out.contains("\"error\":\"HTTP 404\""));
    }

    #[test]
    fn confluence_user_get_results_jsonl() {
        use crate::atlassian::confluence_types::{ConfluenceUserGetResults, ConfluenceUserRecord};
        let list = ConfluenceUserGetResults {
            users: vec![ConfluenceUserRecord {
                account_id: "abc".to_string(),
                display_name: Some("Alice".to_string()),
                email: Some("a@x.com".to_string()),
                account_type: Some("atlassian".to_string()),
                active: None,
                error: None,
            }],
        };
        let out = jsonl_string(&list);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"account_id\":\"abc\""));
    }

    #[test]
    fn jira_dev_status_jsonl_single_line() {
        let status = JiraDevStatus {
            pull_requests: vec![],
            branches: vec![],
            repositories: vec![],
        };
        let out = jsonl_string(&status);
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn jira_dev_status_summary_jsonl_single_line() {
        let summary = JiraDevStatusSummary {
            pullrequest: JiraDevStatusCount {
                count: 0,
                providers: vec![],
            },
            branch: JiraDevStatusCount {
                count: 0,
                providers: vec![],
            },
            repository: JiraDevStatusCount {
                count: 0,
                providers: vec![],
            },
        };
        let out = jsonl_string(&summary);
        assert_eq!(out.lines().count(), 1);
    }

    // ── output_as / write_output over wrapper types ────────────────

    #[test]
    fn output_as_jsonl_wrapper_returns_true() {
        let list = AgileBoardList {
            boards: vec![sample_board(1, "b")],
            total: 1,
        };
        assert!(output_as(&list, &OutputFormat::Jsonl).unwrap());
    }

    #[test]
    fn write_output_jsonl_wrapper() {
        let list = AgileBoardList {
            boards: vec![sample_board(1, "b1"), sample_board(2, "b2")],
            total: 2,
        };
        let mut buf = Vec::new();
        let wrote = write_output(&list, &OutputFormat::Jsonl, &mut buf).unwrap();
        assert!(wrote);
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"id\":1"));
        assert!(out.contains("\"id\":2"));
    }

    #[test]
    fn write_output_json_wrapper_includes_total_field() {
        let list = AgileBoardList {
            boards: vec![sample_board(1, "b1")],
            total: 42,
        };
        let mut buf = Vec::new();
        write_output(&list, &OutputFormat::Json, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\"total\": 42"));
    }

    #[test]
    fn write_output_yaml_wrapper_includes_total_field() {
        let list = AgileBoardList {
            boards: vec![],
            total: 0,
        };
        let mut buf = Vec::new();
        write_output(&list, &OutputFormat::Yaml, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("total: 0"));
    }
}
