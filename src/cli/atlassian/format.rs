//! Shared format types for Atlassian CLI commands.

use std::io::Write;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

use crate::atlassian::client::{
    AgileBoardList, AgileSprintList, ConfluenceSearchResults, ConfluenceUserSearchResults,
    JiraDevStatus, JiraDevStatusSummary, JiraProjectList, JiraSearchResult, JiraWatcherList,
    JiraWorklogList,
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

/// Display format for list/table commands.
#[derive(Clone, Debug, Default, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table.
    #[default]
    Table,
    /// JSON.
    Json,
    /// YAML (single document).
    Yaml,
    /// YAML stream (`---`-separated multi-document).
    Yamls,
    /// JSON Lines: one compact JSON object per line, streaming-friendly.
    Jsonl,
}

/// Writes a value as newline-terminated JSON Lines.
///
/// For collection-like types, implementations emit one JSON object per
/// contained item. For scalar types, implementations emit the value as a
/// single JSON line.
pub trait JsonlSerialize {
    /// Writes the value as JSONL to `out`, newline-terminated.
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()>;
}

/// Writes each item in an iterator as a single compact JSON line.
pub fn write_items_jsonl<'a, I, T>(items: I, out: &mut dyn Write) -> Result<()>
where
    I: IntoIterator<Item = &'a T>,
    T: Serialize + 'a,
{
    for item in items {
        let line = serde_json::to_string(item).context("Failed to serialize as JSON")?;
        writeln!(out, "{line}").context("Failed to write JSONL line")?;
    }
    Ok(())
}

/// Writes a single serializable value as one compact JSON line.
pub fn write_scalar_jsonl<T: Serialize>(item: &T, out: &mut dyn Write) -> Result<()> {
    let line = serde_json::to_string(item).context("Failed to serialize as JSON")?;
    writeln!(out, "{line}").context("Failed to write JSONL line")?;
    Ok(())
}

impl<T: Serialize> JsonlSerialize for Vec<T> {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.iter(), out)
    }
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

/// Writes `data` to `out` in the requested format.
///
/// Returns `Ok(true)` when `data` was written (json/yaml/yamls/jsonl), `Ok(false)`
/// when `format` is `Table` (the caller is expected to render its own table).
pub fn write_output<T: Serialize + JsonlSerialize>(
    data: &T,
    format: &OutputFormat,
    out: &mut dyn Write,
) -> Result<bool> {
    match format {
        OutputFormat::Table => Ok(false),
        OutputFormat::Json => {
            let rendered =
                serde_json::to_string_pretty(data).context("Failed to serialize as JSON")?;
            writeln!(out, "{rendered}").context("Failed to write JSON output")?;
            Ok(true)
        }
        OutputFormat::Yaml => {
            let rendered = serde_yaml::to_string(data).context("Failed to serialize as YAML")?;
            write!(out, "{rendered}").context("Failed to write YAML output")?;
            Ok(true)
        }
        OutputFormat::Yamls => {
            let rendered = format_yaml_stream(data)?;
            write!(out, "{rendered}").context("Failed to write YAML stream output")?;
            Ok(true)
        }
        OutputFormat::Jsonl => {
            data.write_jsonl(out)?;
            Ok(true)
        }
    }
}

/// Serializes a single YAML value as one `---`-prefixed document.
fn yaml_stream_doc(value: &serde_yaml::Value) -> Result<String> {
    let s = serde_yaml::to_string(value).context("Failed to serialize YAML stream item")?;
    Ok(format!("---\n{s}"))
}

/// Serializes data as a YAML multi-document stream.
///
/// If the serialized value is a sequence, each element is emitted as its own
/// `---`-prefixed YAML document. Otherwise the whole value is emitted as a
/// single `---`-prefixed document. The result always ends with a newline.
fn format_yaml_stream<T: Serialize>(data: &T) -> Result<String> {
    match serde_yaml::to_value(data).context("Failed to serialize as YAML stream")? {
        serde_yaml::Value::Sequence(items) => items.iter().map(yaml_stream_doc).collect(),
        other => yaml_stream_doc(&other),
    }
}

/// Serializes data in the requested output format to stdout.
/// Returns `Ok(true)` if data was printed (json/yaml/yamls/jsonl), `Ok(false)`
/// if the caller should handle table output.
pub fn output_as<T: Serialize + JsonlSerialize>(data: &T, format: &OutputFormat) -> Result<bool> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    write_output(data, format, &mut handle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::{
        AgileBoard, AgileSprint, ConfluenceSearchResult, ConfluenceUserSearchResult, JiraComment,
        JiraDevStatusCount, JiraIssue, JiraProject, JiraUser, JiraWorklog,
    };

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
        let cloned = format.clone();
        assert!(matches!(cloned, ContentFormat::Adf));
    }

    // ── OutputFormat ───────────────────────────────────────────────

    #[test]
    fn output_default_is_table() {
        assert!(matches!(OutputFormat::default(), OutputFormat::Table));
    }

    #[test]
    fn output_json_variant() {
        assert!(matches!(OutputFormat::Json, OutputFormat::Json));
    }

    #[test]
    fn output_yaml_variant() {
        assert!(matches!(OutputFormat::Yaml, OutputFormat::Yaml));
    }

    #[test]
    fn output_yamls_variant() {
        assert!(matches!(OutputFormat::Yamls, OutputFormat::Yamls));
    }

    #[test]
    fn output_jsonl_variant() {
        assert!(matches!(OutputFormat::Jsonl, OutputFormat::Jsonl));
    }

    #[test]
    fn output_debug_format() {
        assert_eq!(format!("{:?}", OutputFormat::Jsonl), "Jsonl");
    }

    #[test]
    fn output_clone() {
        let format = OutputFormat::Jsonl;
        let cloned = format.clone();
        assert!(matches!(cloned, OutputFormat::Jsonl));
    }

    // ── output_as ──────────────────────────────────────────────────

    #[test]
    fn output_as_table_returns_false() {
        let data = vec![1, 2, 3];
        assert!(!output_as(&data, &OutputFormat::Table).unwrap());
    }

    #[test]
    fn output_as_json_returns_true() {
        let data = vec![1, 2, 3];
        assert!(output_as(&data, &OutputFormat::Json).unwrap());
    }

    #[test]
    fn output_as_yaml_returns_true() {
        let data = vec![1, 2, 3];
        assert!(output_as(&data, &OutputFormat::Yaml).unwrap());
    }

    #[test]
    fn output_as_yamls_returns_true() {
        let data = vec![1, 2, 3];
        assert!(output_as(&data, &OutputFormat::Yamls).unwrap());
    }

    #[test]
    fn output_as_jsonl_returns_true() {
        let data = vec![1, 2, 3];
        assert!(output_as(&data, &OutputFormat::Jsonl).unwrap());
    }

    // ── write_items_jsonl / Vec impl ───────────────────────────────

    #[test]
    fn vec_jsonl_empty_emits_nothing() {
        let data: Vec<i32> = vec![];
        let mut buf = Vec::new();
        data.write_jsonl(&mut buf).unwrap();
        assert_eq!(buf, b"");
    }

    #[test]
    fn vec_jsonl_emits_one_line_per_item() {
        let data = vec![1_i32, 2, 3];
        let mut buf = Vec::new();
        data.write_jsonl(&mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "1\n2\n3\n");
    }

    #[test]
    fn vec_jsonl_emits_compact_objects() {
        #[derive(Serialize)]
        struct Item {
            key: &'static str,
            val: u32,
        }
        let data = vec![Item { key: "a", val: 1 }, Item { key: "b", val: 2 }];
        let mut buf = Vec::new();
        data.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(
            out,
            "{\"key\":\"a\",\"val\":1}\n{\"key\":\"b\",\"val\":2}\n"
        );
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
    fn write_items_jsonl_over_slice() {
        let data = [10_i32, 20];
        let mut buf = Vec::new();
        write_items_jsonl(data.iter(), &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "10\n20\n");
    }

    #[test]
    fn write_scalar_jsonl_emits_one_line() {
        #[derive(Serialize)]
        struct Scalar {
            name: &'static str,
        }
        let item = Scalar { name: "solo" };
        let mut buf = Vec::new();
        write_scalar_jsonl(&item, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"name\":\"solo\"}\n");
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
        }
    }

    // ── wrapper type JsonlSerialize impls ──────────────────────────

    fn jsonl_string<T: JsonlSerialize>(value: &T) -> String {
        let mut buf = Vec::new();
        value.write_jsonl(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
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

    // ── output_as jsonl round-trip ─────────────────────────────────

    #[test]
    fn output_as_jsonl_empty_vec_returns_true() {
        let data: Vec<i32> = vec![];
        assert!(output_as(&data, &OutputFormat::Jsonl).unwrap());
    }

    #[test]
    fn output_as_jsonl_wrapper_returns_true() {
        let list = AgileBoardList {
            boards: vec![sample_board(1, "b")],
            total: 1,
        };
        assert!(output_as(&list, &OutputFormat::Jsonl).unwrap());
    }

    // ── format_yaml_stream ─────────────────────────────────────────

    #[derive(serde::Serialize)]
    struct Issue {
        key: &'static str,
        summary: &'static str,
    }

    #[test]
    fn yaml_stream_emits_one_doc_per_sequence_item() {
        let data = vec![
            Issue {
                key: "PROJ-1",
                summary: "Fix login",
            },
            Issue {
                key: "PROJ-2",
                summary: "Add feature",
            },
        ];
        let out = format_yaml_stream(&data).unwrap();
        assert_eq!(
            out,
            "---\nkey: PROJ-1\nsummary: Fix login\n---\nkey: PROJ-2\nsummary: Add feature\n"
        );
    }

    #[test]
    fn yaml_stream_empty_sequence_emits_nothing() {
        let data: Vec<Issue> = vec![];
        let out = format_yaml_stream(&data).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn yaml_stream_single_item_sequence() {
        let data = vec![Issue {
            key: "PROJ-1",
            summary: "Fix login",
        }];
        let out = format_yaml_stream(&data).unwrap();
        assert_eq!(out, "---\nkey: PROJ-1\nsummary: Fix login\n");
    }

    #[test]
    fn yaml_stream_non_sequence_emits_single_doc() {
        let data = Issue {
            key: "PROJ-1",
            summary: "Fix login",
        };
        let out = format_yaml_stream(&data).unwrap();
        assert_eq!(out, "---\nkey: PROJ-1\nsummary: Fix login\n");
    }

    #[test]
    fn yaml_stream_scalar_emits_single_doc() {
        let data: i32 = 42;
        let out = format_yaml_stream(&data).unwrap();
        assert_eq!(out, "---\n42\n");
    }

    #[test]
    fn yaml_stream_nested_sequences_treat_outer_only() {
        let data = vec![vec![1, 2], vec![3, 4]];
        let out = format_yaml_stream(&data).unwrap();
        assert_eq!(out, "---\n- 1\n- 2\n---\n- 3\n- 4\n");
    }

    #[test]
    fn yaml_stream_round_trips_via_safe_load_all() {
        use serde::Deserialize;

        let data = vec![
            Issue {
                key: "PROJ-1",
                summary: "Fix login",
            },
            Issue {
                key: "PROJ-2",
                summary: "Add feature",
            },
        ];
        let out = format_yaml_stream(&data).unwrap();

        let docs: Vec<serde_yaml::Value> = serde_yaml::Deserializer::from_str(&out)
            .map(serde_yaml::Value::deserialize)
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["key"], serde_yaml::Value::from("PROJ-1"));
        assert_eq!(docs[1]["key"], serde_yaml::Value::from("PROJ-2"));
    }

    // ── write_output ───────────────────────────────────────────────

    #[test]
    fn write_output_table_returns_false_and_writes_nothing() {
        let data = vec![1_i32, 2];
        let mut buf = Vec::new();
        let wrote = write_output(&data, &OutputFormat::Table, &mut buf).unwrap();
        assert!(!wrote);
        assert!(buf.is_empty());
    }

    #[test]
    fn write_output_json_emits_pretty_array() {
        let data = vec![1_i32, 2, 3];
        let mut buf = Vec::new();
        let wrote = write_output(&data, &OutputFormat::Json, &mut buf).unwrap();
        assert!(wrote);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with('['));
        assert!(out.contains("  1,\n"));
        assert!(out.ends_with("]\n"));
    }

    #[test]
    fn write_output_yaml_emits_list() {
        let data = vec![1_i32, 2];
        let mut buf = Vec::new();
        let wrote = write_output(&data, &OutputFormat::Yaml, &mut buf).unwrap();
        assert!(wrote);
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "- 1\n- 2\n");
    }

    #[test]
    fn write_output_yamls_emits_yaml_stream() {
        let data = vec![1_i32, 2];
        let mut buf = Vec::new();
        let wrote = write_output(&data, &OutputFormat::Yamls, &mut buf).unwrap();
        assert!(wrote);
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "---\n1\n---\n2\n");
    }

    #[test]
    fn write_output_jsonl_emits_one_line_per_item() {
        let data = vec![1_i32, 2, 3];
        let mut buf = Vec::new();
        let wrote = write_output(&data, &OutputFormat::Jsonl, &mut buf).unwrap();
        assert!(wrote);
        assert_eq!(String::from_utf8(buf).unwrap(), "1\n2\n3\n");
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

    #[test]
    fn write_output_propagates_write_errors() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("boom"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let data = vec![1_i32];
        let mut writer = FailingWriter;

        assert!(write_output(&data, &OutputFormat::Json, &mut writer).is_err());
        assert!(write_output(&data, &OutputFormat::Yaml, &mut writer).is_err());
        assert!(write_output(&data, &OutputFormat::Yamls, &mut writer).is_err());
        assert!(write_output(&data, &OutputFormat::Jsonl, &mut writer).is_err());
    }

    #[test]
    fn write_scalar_jsonl_propagates_write_errors() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("boom"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let status = JiraDevStatus {
            pull_requests: vec![],
            branches: vec![],
            repositories: vec![],
        };
        let mut writer = FailingWriter;
        assert!(write_output(&status, &OutputFormat::Jsonl, &mut writer).is_err());
    }
}
