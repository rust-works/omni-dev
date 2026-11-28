//! Timestamps written in the forms omni-dev has always written them (#2257).
//!
//! omni-dev moved from `chrono` to [`jiff`], and the two print the same instant
//! differently. chrono writes a fraction with 0, 3, 6 or 9 digits, whichever is
//! the fewest that hold it; jiff writes the fewest digits outright, so
//! `.123450` would become `.12345`. The timestamps here are read back by other
//! builds (the persisted files, the daemon's replies, the request log), so every
//! place that writes one goes through this module:
//!
//! - [`rfc3339`]: `2026-10-07T12:00:00.123450Z`, the serde form.
//! - [`rfc3339_offset`]: the same instant with `+00:00` for `Z`.
//! - [`rfc3339_secs`] / [`rfc3339_millis`]: a fixed 0 or 3 fraction digits,
//!   truncated rather than rounded.
//! - [`serde_rfc3339`]: the `#[serde(with = ...)]` module every serialized
//!   [`Timestamp`] field uses. jiff's own serde impl writes the fewest
//!   fraction digits, so a field without the attribute would change its output
//!   with no compile error; a test in this module fails when one is missing.
//! - [`OffsetTimestamp`]: an instant that keeps the UTC offset it was recorded
//!   in, for a commit's author date.

use std::fmt;
use std::str::FromStr;

use jiff::fmt::temporal::{DateTimePrinter, Pieces};
use jiff::tz::Offset;
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The fewest of 0, 3, 6 or 9 fraction digits that hold `ts` exactly.
fn auto_precision(ts: Timestamp) -> u8 {
    auto_digits(ts.subsec_nanosecond())
}

/// The fewest of 0, 3, 6 or 9 fraction digits that hold `subsec_nanos`
/// exactly: chrono's "AutoSi" width, which every timestamp here is written in.
pub(crate) fn auto_digits(subsec_nanos: i32) -> u8 {
    // A pre-epoch timestamp's sub-second part is negative; its civil fraction
    // is the complement, which is divisible by the same powers of ten.
    let nanos = subsec_nanos.rem_euclid(1_000_000_000);
    if nanos == 0 {
        0
    } else if nanos % 1_000_000 == 0 {
        3
    } else if nanos % 1_000 == 0 {
        6
    } else {
        9
    }
}

/// A printer writing exactly `precision` fraction digits, truncating.
pub(crate) fn printer(precision: u8) -> DateTimePrinter {
    DateTimePrinter::new().precision(Some(precision))
}

/// `ts` as RFC 3339 in UTC with `Z`, and 0, 3, 6 or 9 fraction digits:
/// `2026-10-07T12:00:00.123450Z`. The form of every serialized timestamp.
#[must_use]
pub fn rfc3339(ts: Timestamp) -> String {
    printer(auto_precision(ts)).timestamp_to_string(&ts)
}

/// `ts` as [`rfc3339`] but with a `+00:00` offset instead of `Z`:
/// `2026-10-07T12:00:00.123450+00:00`.
#[must_use]
pub fn rfc3339_offset(ts: Timestamp) -> String {
    printer(auto_precision(ts)).timestamp_with_offset_to_string(&ts, Offset::UTC)
}

/// `ts` as RFC 3339 in UTC with `Z` and whole seconds, the fraction truncated:
/// `2026-10-07T12:00:00Z`.
#[must_use]
pub fn rfc3339_secs(ts: Timestamp) -> String {
    printer(0).timestamp_to_string(&ts)
}

/// `ts` as RFC 3339 in UTC with `Z` and exactly three fraction digits,
/// truncated: `2026-10-07T12:00:00.120Z`.
#[must_use]
pub fn rfc3339_millis(ts: Timestamp) -> String {
    printer(3).timestamp_to_string(&ts)
}

/// Parses a `YYYY-MM-DD` date as the instant of its midnight in UTC.
///
/// # Errors
///
/// When `s` is not such a date.
pub fn parse_date_as_utc_midnight(s: &str) -> Result<Timestamp, jiff::Error> {
    let date = jiff::civil::Date::strptime("%Y-%m-%d", s)?;
    Offset::UTC.to_timestamp(date.to_datetime(jiff::civil::Time::midnight()))
}

/// `#[serde(with = "crate::utils::time::serde_rfc3339")]` for a [`Timestamp`]
/// field: writes [`rfc3339`], and reads any RFC 3339 timestamp with an offset
/// (`Z` or numeric), normalized to the instant.
pub mod serde_rfc3339 {
    use super::{rfc3339, Deserialize, Deserializer, Serializer, Timestamp};

    /// Writes `ts` as [`rfc3339`].
    ///
    /// # Errors
    ///
    /// Only the serializer's own errors.
    pub fn serialize<S: Serializer>(ts: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&rfc3339(*ts))
    }

    /// Reads an RFC 3339 timestamp.
    ///
    /// # Errors
    ///
    /// When the value is not a string holding an RFC 3339 timestamp with an
    /// offset.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        Timestamp::deserialize(deserializer)
    }

    /// The same for an `Option<Timestamp>` field. A missing field still needs
    /// `#[serde(default)]`: serde treats an absent `Option` as `None` only when
    /// it deserializes the field itself.
    pub mod option {
        use super::super::{Deserialize, Deserializer, Serialize, Serializer, Timestamp};

        struct Rfc3339(Timestamp);

        impl Serialize for Rfc3339 {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                super::serialize(&self.0, serializer)
            }
        }

        /// Writes `Some(ts)` as [`rfc3339`](super::super::rfc3339), `None` as
        /// null.
        ///
        /// # Errors
        ///
        /// Only the serializer's own errors.
        #[allow(clippy::ref_option)] // serde's `with` passes `&Option<T>`.
        pub fn serialize<S: Serializer>(
            ts: &Option<Timestamp>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match ts {
                Some(ts) => serializer.serialize_some(&Rfc3339(*ts)),
                None => serializer.serialize_none(),
            }
        }

        /// Reads an RFC 3339 timestamp, or null.
        ///
        /// # Errors
        ///
        /// When the value is neither null nor an RFC 3339 timestamp with an
        /// offset.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Timestamp>, D::Error> {
            Option::<Timestamp>::deserialize(deserializer)
        }
    }
}

/// An instant together with the UTC offset it was recorded in: a commit's
/// author date.
///
/// Written as RFC 3339 in that offset with [`rfc3339`]'s fraction rule, and a
/// zero offset written as `Z`: `2024-01-01T10:00:00+10:00`,
/// `2024-01-01T00:00:00Z`.
///
/// Compared, ordered and hashed by the instant alone, as chrono's
/// `DateTime<FixedOffset>` was: `10:00+10:00` equals `00:00Z`.
#[derive(Debug, Clone, Copy)]
pub struct OffsetTimestamp {
    timestamp: Timestamp,
    offset: Offset,
}

impl OffsetTimestamp {
    /// `timestamp` as seen at `offset`.
    #[must_use]
    pub const fn new(timestamp: Timestamp, offset: Offset) -> Self {
        Self { timestamp, offset }
    }

    /// The instant.
    #[must_use]
    pub const fn timestamp(self) -> Timestamp {
        self.timestamp
    }

    /// The offset the instant was recorded in.
    #[must_use]
    pub const fn offset(self) -> Offset {
        self.offset
    }
}

impl PartialEq for OffsetTimestamp {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp
    }
}

impl Eq for OffsetTimestamp {}

impl PartialOrd for OffsetTimestamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OffsetTimestamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp.cmp(&other.timestamp)
    }
}

impl std::hash::Hash for OffsetTimestamp {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.timestamp.hash(state);
    }
}

impl From<Timestamp> for OffsetTimestamp {
    /// `timestamp` at UTC.
    fn from(timestamp: Timestamp) -> Self {
        Self::new(timestamp, Offset::UTC)
    }
}

impl fmt::Display for OffsetTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let printer = printer(auto_precision(self.timestamp));
        if self.offset == Offset::UTC {
            f.write_str(&printer.timestamp_to_string(&self.timestamp))
        } else {
            f.write_str(&printer.timestamp_with_offset_to_string(&self.timestamp, self.offset))
        }
    }
}

impl FromStr for OffsetTimestamp {
    type Err = jiff::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let timestamp: Timestamp = s.parse()?;
        // The instant parsed, so the offset is there; `Z` reads as zero.
        let offset = Pieces::parse(s)?
            .offset()
            .map_or(Offset::UTC, |offset| offset.to_numeric_offset());
        Ok(Self::new(timestamp, offset))
    }
}

impl Serialize for OffsetTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OffsetTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::Path;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn rfc3339_writes_the_fewest_of_0_3_6_or_9_fraction_digits() {
        for (input, expected) in crate::test_support::timestamp_forms::UTC {
            assert_eq!(rfc3339(ts(input)), *expected, "{input}");
        }
    }

    #[test]
    fn rfc3339_writes_a_pre_epoch_fraction_by_the_same_rule() {
        assert_eq!(
            rfc3339(ts("1969-12-31T23:59:59.5Z")),
            "1969-12-31T23:59:59.500Z"
        );
        assert_eq!(
            rfc3339(ts("1969-12-31T23:59:59.99999Z")),
            "1969-12-31T23:59:59.999990Z"
        );
        assert_eq!(rfc3339(ts("1969-12-31T23:59:59Z")), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn rfc3339_offset_writes_plus_zero_instead_of_z() {
        assert_eq!(
            rfc3339_offset(ts("2023-11-14T22:13:20Z")),
            "2023-11-14T22:13:20+00:00"
        );
        assert_eq!(
            rfc3339_offset(ts("2023-11-14T22:13:20.12345Z")),
            "2023-11-14T22:13:20.123450+00:00"
        );
    }

    #[test]
    fn fixed_precision_forms_truncate() {
        let t = ts("2026-10-07T12:00:00.999999999Z");
        assert_eq!(rfc3339_secs(t), "2026-10-07T12:00:00Z");
        assert_eq!(rfc3339_millis(t), "2026-10-07T12:00:00.999Z");
        assert_eq!(
            rfc3339_millis(ts("2026-10-07T12:00:00Z")),
            "2026-10-07T12:00:00.000Z"
        );
        assert_eq!(
            rfc3339_secs(ts("1969-12-31T23:59:59.5Z")),
            "1969-12-31T23:59:59Z"
        );
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Fields {
        #[serde(with = "serde_rfc3339")]
        at: Timestamp,
        #[serde(default, with = "serde_rfc3339::option")]
        maybe: Option<Timestamp>,
    }

    #[test]
    fn the_serde_modules_round_trip() {
        let template = serde_json::json!({ "at": "2026-01-01T00:00:00Z", "maybe": null });
        crate::test_support::timestamp_forms::assert_round_trips::<Fields>(&template, "/at");
        let template = serde_json::json!({ "at": "2026-01-01T00:00:00Z", "maybe": "x" });
        crate::test_support::timestamp_forms::assert_round_trips::<Fields>(&template, "/maybe");
    }

    #[test]
    fn an_absent_or_null_option_reads_as_none() {
        let absent: Fields = serde_json::from_str(r#"{"at":"2026-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(absent.maybe, None);
        let null: Fields =
            serde_json::from_str(r#"{"at":"2026-01-01T00:00:00Z","maybe":null}"#).unwrap();
        assert_eq!(null.maybe, None);
        assert_eq!(
            serde_json::to_string(&null).unwrap(),
            r#"{"at":"2026-01-01T00:00:00Z","maybe":null}"#
        );
    }

    #[test]
    fn a_timestamp_without_an_offset_is_rejected() {
        assert!(serde_json::from_str::<Fields>(r#"{"at":"2026-01-01T00:00:00"}"#).is_err());
        assert!(serde_json::from_str::<Fields>(r#"{"at":"yesterday"}"#).is_err());
    }

    #[test]
    fn yaml_reads_and_writes_the_same_form() {
        let fields = Fields {
            at: ts("2026-01-01T00:00:00.5Z"),
            maybe: None,
        };
        let yaml = serde_norway::to_string(&fields).unwrap();
        assert_eq!(yaml, "at: 2026-01-01T00:00:00.500Z\nmaybe: null\n");
        assert_eq!(serde_norway::from_str::<Fields>(&yaml).unwrap(), fields);
    }

    #[test]
    fn an_offset_timestamp_keeps_its_offset() {
        for (input, expected) in crate::test_support::timestamp_forms::OFFSET {
            let parsed: OffsetTimestamp = input.parse().unwrap();
            assert_eq!(parsed.to_string(), *expected, "{input}");
            assert_eq!(parsed.timestamp(), ts(input), "{input}");
        }
        let parsed: OffsetTimestamp = "2024-01-01T10:00:00+10:00".parse().unwrap();
        assert_eq!(parsed.offset(), Offset::from_hours(10).unwrap());
        assert_eq!(parsed.timestamp(), ts("2024-01-01T00:00:00Z"));
        assert!("2024-01-01T10:00:00".parse::<OffsetTimestamp>().is_err());
    }

    #[test]
    fn offset_timestamps_compare_by_instant() {
        let east: OffsetTimestamp = "2024-01-01T10:00:00+10:00".parse().unwrap();
        let utc: OffsetTimestamp = "2024-01-01T00:00:00Z".parse().unwrap();
        let later: OffsetTimestamp = "2024-01-01T00:00:01-05:00".parse().unwrap();
        assert_eq!(east, utc);
        assert_ne!(east.offset(), utc.offset());
        assert!(utc < later && east < later);
        let set: std::collections::HashSet<_> = [east, utc].into_iter().collect();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn an_offset_timestamp_round_trips_through_serde() {
        let parsed: OffsetTimestamp =
            serde_json::from_str(r#""2024-01-01T00:00:00.5-05:30""#).unwrap();
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            r#""2024-01-01T00:00:00.500-05:30""#
        );
        assert!(serde_json::from_str::<OffsetTimestamp>("42").is_err());
    }

    // ── Guard: every serialized `Timestamp` field writes the pinned form ──

    /// `(line, field)` of every `Timestamp` field, in a type deriving
    /// `Serialize` or `Deserialize`, whose attributes neither route it through
    /// [`serde_rfc3339`] nor skip it.
    fn unpinned_timestamp_fields(source: &str) -> Vec<(usize, String)> {
        let field = regex::Regex::new(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:\s*(?:Option\s*<\s*)?(?:jiff::)?Timestamp\b",
        )
        .unwrap();
        let mut found = Vec::new();
        let mut serde_derive_pending = false;
        // Brace depth inside the current serde type, when in one.
        let mut serde_depth: Option<i32> = None;
        let mut attributes = String::new();
        let mut in_attribute = false;
        for (index, raw) in source.lines().enumerate() {
            let line = raw.trim();
            if line.starts_with("//") {
                continue;
            }
            if let Some(depth) = serde_depth.as_mut() {
                if in_attribute || line.starts_with("#[") {
                    // rustfmt wraps a long attribute over several lines.
                    attributes.push_str(line);
                    in_attribute = !line.ends_with(']');
                } else if let Some(name) = field.captures(line).map(|c| c[1].to_string()) {
                    let pinned = attributes.contains("serde_rfc3339")
                        || attributes.contains("serde(skip)")
                        || attributes.contains("serde(skip,");
                    if *depth == 1 && !pinned {
                        found.push((index + 1, name));
                    }
                    attributes.clear();
                } else if !line.is_empty() {
                    attributes.clear();
                }
                *depth += brace_delta(line);
                if *depth <= 0 {
                    serde_depth = None;
                }
                continue;
            }
            if line.starts_with("#[derive(")
                && (line.contains("Serialize") || line.contains("Deserialize"))
            {
                serde_derive_pending = true;
            } else if serde_derive_pending && !line.starts_with("#[") && !line.is_empty() {
                serde_derive_pending = false;
                let is_type = line.contains("struct ") || line.contains("enum ");
                if is_type && line.contains('{') {
                    let depth = brace_delta(line);
                    if depth > 0 {
                        serde_depth = Some(depth);
                        attributes.clear();
                    }
                }
            }
        }
        found
    }

    fn brace_delta(line: &str) -> i32 {
        let opens = line.matches('{').count();
        let closes = line.matches('}').count();
        i32::try_from(opens).unwrap() - i32::try_from(closes).unwrap()
    }

    #[test]
    fn the_guard_flags_only_unpinned_fields() {
        let source = r#"
#[derive(Debug, Serialize)]
pub struct Bad {
    /// Doc.
    pub at: Timestamp,
    #[serde(default)]
    maybe: Option<Timestamp>,
}

#[derive(Serialize, Deserialize)]
struct Good {
    #[serde(with = "crate::utils::time::serde_rfc3339")]
    at: Timestamp,
    #[serde(default, with = "crate::utils::time::serde_rfc3339::option")]
    maybe: Option<Timestamp>,
    #[serde(skip)]
    skipped: Timestamp,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::utils::time::serde_rfc3339::option"
    )]
    wrapped: Option<Timestamp>,
}

#[derive(Debug, Clone)]
struct NotSerde {
    at: Timestamp,
}

#[derive(Serialize)]
enum Variants {
    One {
        at: jiff::Timestamp,
    },
}
"#;
        let names: Vec<String> = unpinned_timestamp_fields(source)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(names, vec!["at", "maybe"]);
    }

    #[test]
    fn every_serialized_timestamp_field_uses_the_pinned_form() {
        fn visit(dir: &Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, offenders);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    // This file's own guard test holds deliberately unpinned
                    // fields in a sample string.
                    && !path.ends_with("utils/time.rs")
                {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for (line, name) in unpinned_timestamp_fields(&source) {
                        offenders.push(format!("{}:{line} `{name}`", path.display()));
                    }
                }
            }
        }
        let mut offenders = Vec::new();
        visit(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut offenders,
        );
        assert!(
            offenders.is_empty(),
            "serialized Timestamp fields without \
             `#[serde(with = \"crate::utils::time::serde_rfc3339\")]` (jiff's own \
             serde impl writes a different fraction width, #2257):\n{}",
            offenders.join("\n")
        );
    }
}
