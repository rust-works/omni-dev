//! Result rows and arbitrary-schema → JSON decoding.
//!
//! Snowflake's v1 query endpoint returns column metadata (`rowtype`) plus a
//! row set whose cells are stringified (or null). This module models a column,
//! a row, and the mapping from a raw cell to a `serde_json::Value` driven by the
//! column's Snowflake type tag — implemented from the documented wire formats
//! (e.g. `DATE` is days-since-epoch, timestamps are `seconds[.fraction]`).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Days, NaiveDate, NaiveTime, Utc};
use serde_json::{json, Map, Value};

/// Metadata for one result column, from the query response `rowtype`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// Column name as Snowflake reported it.
    pub name: String,
    /// Lowercase Snowflake type tag (e.g. `fixed`, `text`, `timestamp_ntz`).
    pub ty: String,
    /// Whether the column may be null.
    pub nullable: bool,
    /// Declared length (text), when reported.
    pub length: Option<i64>,
    /// Declared precision (number), when reported.
    pub precision: Option<i64>,
    /// Declared scale (number/time/timestamp), when reported.
    pub scale: Option<i64>,
}

impl Column {
    /// A compact, lowercase type label with parameters when reported (e.g.
    /// `fixed(10,2)`, `text(255)`, `timestamp_ntz(9)`).
    #[must_use]
    pub fn type_label(&self) -> String {
        match (self.precision, self.scale, self.length) {
            (Some(p), Some(s), _) => format!("{}({p},{s})", self.ty),
            (Some(p), None, _) => format!("{}({p})", self.ty),
            (None, Some(s), _) => format!("{}({s})", self.ty),
            (None, None, Some(l)) => format!("{}({l})", self.ty),
            _ => self.ty.clone(),
        }
    }
}

/// One result row: raw stringified cells plus the shared column schema.
#[derive(Clone, Debug)]
pub struct Row {
    values: Vec<Option<String>>,
    columns: Arc<Vec<Column>>,
    index: Arc<HashMap<String, usize>>,
}

impl Row {
    /// Builds a row from its raw cells and the shared schema/index.
    #[must_use]
    pub fn new(
        values: Vec<Option<String>>,
        columns: Arc<Vec<Column>>,
        index: Arc<HashMap<String, usize>>,
    ) -> Self {
        Self {
            values,
            columns,
            index,
        }
    }

    /// The shared column schema.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The raw cell value at `index`, if present.
    #[must_use]
    pub fn raw_at(&self, index: usize) -> Option<&str> {
        self.values.get(index).and_then(|v| v.as_deref())
    }

    /// The raw cell value for a column name (case-insensitive), if present.
    #[must_use]
    pub fn raw(&self, name: &str) -> Option<&str> {
        self.index
            .get(&name.to_ascii_uppercase())
            .and_then(|&i| self.raw_at(i))
    }

    /// Converts the row to a JSON object keyed by column name.
    ///
    /// Repeated column names (e.g. `SELECT 1, 1`) are disambiguated with a
    /// `_<n>` suffix so no column is silently dropped — JSON object keys must be
    /// unique.
    #[must_use]
    pub fn to_json_object(&self) -> Map<String, Value> {
        let mut map = Map::with_capacity(self.columns.len());
        let mut seen: HashMap<&str, u32> = HashMap::new();
        for (i, col) in self.columns.iter().enumerate() {
            let raw = self.values.get(i).and_then(|v| v.as_deref());
            let value = value_to_json(raw, col);
            let count = seen.entry(col.name.as_str()).or_insert(0);
            *count += 1;
            let key = if *count == 1 {
                col.name.clone()
            } else {
                format!("{}_{}", col.name, *count)
            };
            map.insert(key, value);
        }
        map
    }
}

/// Builds the self-describing `{ columns: [{name, type}], rows: [{col: val}] }`
/// payload from a query's rows. Column metadata comes from the first row (an
/// empty result reports `columns: []`).
#[must_use]
pub fn rows_to_payload(rows: &[Row]) -> Value {
    let columns: Vec<Value> = rows.first().map_or_else(Vec::new, |row| {
        row.columns()
            .iter()
            .map(|c| json!({ "name": c.name, "type": c.type_label() }))
            .collect()
    });
    let out: Vec<Value> = rows
        .iter()
        .map(|r| Value::Object(r.to_json_object()))
        .collect();
    json!({ "columns": columns, "rows": out })
}

/// Builds the uniform multi-statement payload from one row set per statement.
///
/// The shape is `{ statements: [ {columns, rows}, … ] }`; a single-statement
/// query is a one-element `statements` array, so every query result is uniform.
#[must_use]
pub fn rows_to_multi_payload(statements: &[Vec<Row>]) -> Value {
    let statements: Vec<Value> = statements
        .iter()
        .map(|rows| rows_to_payload(rows))
        .collect();
    json!({ "statements": statements })
}

/// Maps a single raw cell to JSON according to its column type. Total: any
/// decode failure falls back to the raw string (or `null`).
#[must_use]
pub fn value_to_json(raw: Option<&str>, col: &Column) -> Value {
    let Some(s) = raw else { return Value::Null };
    match col.ty.as_str() {
        "fixed" if col.scale.unwrap_or(0) == 0 => s
            .parse::<i64>()
            .map_or_else(|_| Value::String(s.to_string()), |n| json!(n)),
        "fixed" | "real" | "float" | "double" | "double precision" => number_or_string(s),
        "boolean" => match s {
            "1" => json!(true),
            "0" => json!(false),
            _ if s.eq_ignore_ascii_case("true") => json!(true),
            _ if s.eq_ignore_ascii_case("false") => json!(false),
            _ => Value::String(s.to_string()),
        },
        "date" => decode_date(s),
        "time" => decode_time(s),
        "timestamp_ntz" => decode_epoch(s).map_or_else(
            || Value::String(s.to_string()),
            |dt| json!(dt.naive_utc().format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        ),
        "timestamp_ltz" => decode_epoch(s)
            .map_or_else(|| Value::String(s.to_string()), |dt| json!(dt.to_rfc3339())),
        "timestamp_tz" => decode_timestamp_tz(s),
        "variant" | "object" | "array" => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
        }
        // text/varchar/char/string, binary (hex), geography, geometry, vector,
        // and any unknown type: the raw wire string.
        _ => Value::String(s.to_string()),
    }
}

/// Parses a float string to a JSON number, falling back to the raw string for
/// values JSON cannot represent.
fn number_or_string(s: &str) -> Value {
    s.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or_else(|| Value::String(s.to_string()), Value::Number)
}

/// `DATE` is days since the Unix epoch.
fn decode_date(s: &str) -> Value {
    let Ok(days) = s.parse::<i64>() else {
        return Value::String(s.to_string());
    };
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    let date = if days >= 0 {
        epoch.checked_add_days(Days::new(days.unsigned_abs()))
    } else {
        epoch.checked_sub_days(Days::new(days.unsigned_abs()))
    };
    date.map_or_else(
        || Value::String(s.to_string()),
        |d| json!(d.format("%Y-%m-%d").to_string()),
    )
}

/// `TIME` is seconds-from-midnight with an optional fractional part.
fn decode_time(s: &str) -> Value {
    let Some((secs, nanos)) = split_seconds(s) else {
        return Value::String(s.to_string());
    };
    if secs < 0 {
        return Value::String(s.to_string());
    }
    NaiveTime::from_num_seconds_from_midnight_opt(secs.unsigned_abs() as u32, nanos).map_or_else(
        || Value::String(s.to_string()),
        |t| json!(t.format("%H:%M:%S%.f").to_string()),
    )
}

/// `TIMESTAMP_TZ` wire form is `"<epoch> <offset-minutes-plus-1440>"`; we keep
/// the UTC instant (the offset is advisory and dropped here).
fn decode_timestamp_tz(s: &str) -> Value {
    let epoch = s.split_whitespace().next().unwrap_or(s);
    decode_epoch(epoch).map_or_else(|| Value::String(s.to_string()), |dt| json!(dt.to_rfc3339()))
}

/// Parses an `seconds[.fraction]` epoch value into a UTC instant.
fn decode_epoch(s: &str) -> Option<DateTime<Utc>> {
    let (secs, nanos) = split_seconds(s)?;
    DateTime::from_timestamp(secs, nanos)
}

/// Splits a signed `seconds[.fraction]` string into `(seconds, nanoseconds)`,
/// flooring toward negative infinity so fractional pre-epoch values are correct.
fn split_seconds(s: &str) -> Option<(i64, u32)> {
    let s = s.trim();
    let negative = s.starts_with('-');
    let (secs_str, frac_str) = s.split_once('.').unwrap_or((s, ""));
    let mut secs = secs_str.parse::<i64>().ok()?;
    if frac_str.is_empty() {
        return Some((secs, 0));
    }
    if !frac_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Pad/truncate the fraction to exactly 9 digits (nanoseconds).
    let mut digits: Vec<u8> = frac_str.bytes().take(9).collect();
    digits.resize(9, b'0');
    let mut nanos = std::str::from_utf8(&digits).ok()?.parse::<u32>().ok()?;
    if negative && nanos > 0 {
        // "-1.5s" = floor(-1.5)=-2 plus 0.5s; "-0.x" parses secs as 0.
        secs -= 1;
        nanos = 1_000_000_000 - nanos;
    }
    Some((secs, nanos))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn col(ty: &str, precision: Option<i64>, scale: Option<i64>, length: Option<i64>) -> Column {
        Column {
            name: "C".to_string(),
            ty: ty.to_string(),
            nullable: true,
            length,
            precision,
            scale,
        }
    }

    #[test]
    fn type_label_renders_parameters() {
        assert_eq!(
            col("fixed", Some(10), Some(2), None).type_label(),
            "fixed(10,2)"
        );
        assert_eq!(col("text", None, None, Some(255)).type_label(), "text(255)");
        assert_eq!(
            col("timestamp_ntz", None, None, None).type_label(),
            "timestamp_ntz"
        );
    }

    #[test]
    fn null_is_json_null() {
        assert_eq!(
            value_to_json(None, &col("fixed", None, Some(0), None)),
            Value::Null
        );
    }

    #[test]
    fn decodes_each_type() {
        assert_eq!(
            value_to_json(Some("42"), &col("fixed", Some(38), Some(0), None)),
            json!(42)
        );
        let big = "123456789012345678901234567890";
        assert_eq!(
            value_to_json(Some(big), &col("fixed", Some(38), Some(0), None)),
            json!(big),
            "overflowing integer keeps the exact string"
        );
        assert_eq!(
            value_to_json(Some("123.45"), &col("fixed", Some(10), Some(2), None)),
            json!(123.45)
        );
        assert_eq!(
            value_to_json(Some("1.5"), &col("real", None, None, None)),
            json!(1.5)
        );
        assert_eq!(
            value_to_json(Some("1"), &col("boolean", None, None, None)),
            json!(true)
        );
        assert_eq!(
            value_to_json(Some("0"), &col("boolean", None, None, None)),
            json!(false)
        );
        assert_eq!(
            value_to_json(Some("hello"), &col("text", None, None, Some(255))),
            json!("hello")
        );
        assert_eq!(
            value_to_json(Some("0"), &col("date", None, None, None)),
            json!("1970-01-01")
        );
        assert_eq!(
            value_to_json(Some("19723"), &col("date", None, None, None)),
            json!("2024-01-01")
        );
        assert_eq!(
            value_to_json(Some("3661"), &col("time", None, None, Some(0))),
            json!("01:01:01")
        );
        assert_eq!(
            value_to_json(
                Some("1704067200"),
                &col("timestamp_ntz", None, None, Some(0))
            ),
            json!("2024-01-01T00:00:00")
        );
        assert_eq!(
            value_to_json(
                Some("1704067200"),
                &col("timestamp_ltz", None, None, Some(0))
            ),
            json!("2024-01-01T00:00:00+00:00")
        );
        assert_eq!(
            value_to_json(
                Some("1704067200.000 1440"),
                &col("timestamp_tz", None, None, Some(3))
            ),
            json!("2024-01-01T00:00:00+00:00")
        );
        assert_eq!(
            value_to_json(Some(r#"{"a":1}"#), &col("variant", None, None, None)),
            json!({ "a": 1 })
        );
        assert_eq!(
            value_to_json(Some("DEADBEEF"), &col("binary", None, None, None)),
            json!("DEADBEEF")
        );
    }

    #[test]
    fn malformed_value_falls_back_to_raw() {
        assert_eq!(
            value_to_json(Some("maybe"), &col("boolean", None, None, None)),
            json!("maybe")
        );
    }

    #[test]
    fn row_to_json_keys_by_column_name() {
        let columns = Arc::new(vec![
            Column {
                name: "ID".to_string(),
                ty: "fixed".to_string(),
                nullable: false,
                length: None,
                precision: Some(38),
                scale: Some(0),
            },
            Column {
                name: "NAME".to_string(),
                ty: "text".to_string(),
                nullable: true,
                length: None,
                precision: None,
                scale: None,
            },
        ]);
        let mut index = HashMap::new();
        index.insert("ID".to_string(), 0);
        index.insert("NAME".to_string(), 1);
        let row = Row::new(
            vec![Some("1".to_string()), Some("hi".to_string())],
            columns,
            Arc::new(index),
        );
        let obj = row.to_json_object();
        assert_eq!(obj.get("ID"), Some(&json!(1)));
        assert_eq!(obj.get("NAME"), Some(&json!("hi")));
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn duplicate_column_names_are_disambiguated() {
        let dup = || Column {
            name: "N".to_string(),
            ty: "fixed".to_string(),
            nullable: false,
            length: None,
            precision: Some(38),
            scale: Some(0),
        };
        let columns = Arc::new(vec![dup(), dup()]);
        let mut index = HashMap::new();
        index.insert("N".to_string(), 0);
        let row = Row::new(
            vec![Some("1".to_string()), Some("2".to_string())],
            columns,
            Arc::new(index),
        );
        let obj = row.to_json_object();
        assert_eq!(obj.get("N"), Some(&json!(1)));
        assert_eq!(obj.get("N_2"), Some(&json!(2)), "second N is not dropped");
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn rows_to_multi_payload_wraps_each_statement() {
        let columns = Arc::new(vec![Column {
            name: "N".to_string(),
            ty: "fixed".to_string(),
            nullable: false,
            length: None,
            precision: Some(38),
            scale: Some(0),
        }]);
        let index = Arc::new(HashMap::from([("N".to_string(), 0)]));
        let row = Row::new(vec![Some("1".to_string())], columns, index);

        // Even a single statement is a one-element `statements` array.
        let one = rows_to_multi_payload(&[vec![row.clone()]]);
        assert_eq!(one["statements"].as_array().unwrap().len(), 1);
        assert_eq!(one["statements"][0]["rows"][0]["N"], json!(1));

        // Two statements → two entries, with an empty result set preserved.
        let two = rows_to_multi_payload(&[vec![row], vec![]]);
        let stmts = two["statements"].as_array().unwrap();
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[1], json!({ "columns": [], "rows": [] }));
    }
}
