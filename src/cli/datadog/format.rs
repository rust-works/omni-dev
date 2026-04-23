//! Shared format types for Datadog CLI commands.
//!
//! Duplicated from [`crate::cli::atlassian::format`] intentionally: promoting
//! to a shared module requires decoupling the `JsonlSerialize` trait from
//! Atlassian-specific types, which is out of scope for the Datadog slice.
//! Follow-up: file to extract a shared `src/cli/format.rs`.
//!
//! Items are unused in slice 1 (auth-only); they become live once the
//! metrics / monitor / dashboard / logs subcommands land.
#![allow(dead_code)]

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
/// Returns `Ok(true)` when `data` was written (json/yaml/yamls/jsonl),
/// `Ok(false)` when `format` is `Table` (the caller is expected to render
/// its own table).
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
fn format_yaml_stream<T: Serialize>(data: &T) -> Result<String> {
    match serde_yaml::to_value(data).context("Failed to serialize as YAML stream")? {
        serde_yaml::Value::Sequence(items) => items.iter().map(yaml_stream_doc).collect(),
        other => yaml_stream_doc(&other),
    }
}

/// Serializes data in the requested output format to stdout.
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
    fn output_debug_format() {
        assert_eq!(format!("{:?}", OutputFormat::Jsonl), "Jsonl");
    }

    #[test]
    fn output_clone() {
        let f = OutputFormat::Json.clone();
        assert!(matches!(f, OutputFormat::Json));
    }

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
        assert!(out.ends_with("]\n"));
    }

    #[test]
    fn write_output_yaml_emits_list() {
        let data = vec![1_i32, 2];
        let mut buf = Vec::new();
        write_output(&data, &OutputFormat::Yaml, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "- 1\n- 2\n");
    }

    #[test]
    fn write_output_yamls_emits_stream() {
        let data = vec![1_i32, 2];
        let mut buf = Vec::new();
        write_output(&data, &OutputFormat::Yamls, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "---\n1\n---\n2\n");
    }

    #[test]
    fn write_output_jsonl_emits_one_line_per_item() {
        let data = vec![1_i32, 2, 3];
        let mut buf = Vec::new();
        write_output(&data, &OutputFormat::Jsonl, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "1\n2\n3\n");
    }

    #[test]
    fn output_as_table_returns_false() {
        let data: Vec<i32> = vec![];
        assert!(!output_as(&data, &OutputFormat::Table).unwrap());
    }

    #[test]
    fn output_as_json_returns_true() {
        let data: Vec<i32> = vec![];
        assert!(output_as(&data, &OutputFormat::Json).unwrap());
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
        let mut buf = Vec::new();
        write_scalar_jsonl(&Scalar { name: "solo" }, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"name\":\"solo\"}\n");
    }

    #[test]
    fn yaml_stream_non_sequence_emits_single_doc() {
        #[derive(Serialize)]
        struct S {
            key: &'static str,
        }
        let out = format_yaml_stream(&S { key: "v" }).unwrap();
        assert_eq!(out, "---\nkey: v\n");
    }
}
