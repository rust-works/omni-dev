//! Shared output-format types for the CLI.
//!
//! One `-o/--output <format>` convention is used across the whole CLI surface
//! (issue #1125). This module owns the machine-readable [`OutputFormat`] enum
//! and the rendering machinery (`write_output`/`output_as` and the
//! [`JsonlSerialize`] trait); command modules bind `-o/--output` to this enum
//! and delegate serialization here, rendering their own `Table` branch when
//! `output_as` returns `Ok(false)`.
//!
//! Atlassian-specific `JsonlSerialize` impls for its collection wrapper types
//! live in [`crate::cli::atlassian::format`] (they need the wrapper types); this
//! module carries only the trait, the blanket `Vec<T>` impl, and the generic
//! helpers.

use std::io::Write;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

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

/// A two-way `-o/--output` selector for commands that render either a
/// human-readable table or machine-readable JSON (no YAML/JSONL variants).
///
/// Used by the daemon-facing status/list commands that historically took a
/// boolean `--json` flag (issue #1125).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum TableOrJson {
    /// Human-readable table.
    #[default]
    Table,
    /// Pretty-printed JSON.
    Json,
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
        let cloned = format;
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

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("boom"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("boom"))
        }
    }

    #[test]
    fn write_output_propagates_write_errors() {
        let data = vec![1_i32];
        let mut writer = FailingWriter;

        assert!(write_output(&data, &OutputFormat::Json, &mut writer).is_err());
        assert!(write_output(&data, &OutputFormat::Yaml, &mut writer).is_err());
        assert!(write_output(&data, &OutputFormat::Yamls, &mut writer).is_err());
        assert!(write_output(&data, &OutputFormat::Jsonl, &mut writer).is_err());
        assert!(writer.write(b"x").is_err());
        assert!(writer.flush().is_err());
    }

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("serialize failed"))
        }
    }

    impl JsonlSerialize for FailingSerialize {
        fn write_jsonl(&self, _out: &mut dyn Write) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_output_propagates_json_serialize_errors() {
        let mut buf = Vec::new();
        let err = write_output(&FailingSerialize, &OutputFormat::Json, &mut buf).unwrap_err();
        assert!(err.to_string().contains("Failed to serialize as JSON"));
    }

    #[test]
    fn write_output_propagates_yaml_serialize_errors() {
        let mut buf = Vec::new();
        let err = write_output(&FailingSerialize, &OutputFormat::Yaml, &mut buf).unwrap_err();
        assert!(err.to_string().contains("Failed to serialize as YAML"));
    }

    #[test]
    fn failing_serialize_jsonl_impl_is_a_noop() {
        let mut buf = Vec::new();
        FailingSerialize.write_jsonl(&mut buf).unwrap();
        assert!(buf.is_empty());
    }
}
