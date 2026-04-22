//! Time-range parsing for Datadog CLI commands.
//!
//! Accepts three forms:
//! - Relative shorthand: `{N}{s|m|h|d|w}` (e.g. `15m`, `1h`, `7d`).
//! - Literal `now`.
//! - RFC 3339 timestamps (timezone required, e.g. `2026-04-22T10:00:00Z`).
//! - Unix epoch seconds (non-negative integer).
//!
//! All results are returned as epoch seconds (`i64`) — the unit expected
//! by Datadog v1 query parameters.

use chrono::{DateTime, Utc};

use crate::datadog::error::DatadogError;

/// Parses a single time instant relative to `now`.
///
/// Returns Unix epoch seconds. See module docs for accepted syntax.
pub fn parse_instant(spec: &str, now: DateTime<Utc>) -> Result<i64, DatadogError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(DatadogError::InvalidTimeRange("empty".to_string()));
    }

    if spec.eq_ignore_ascii_case("now") {
        return Ok(now.timestamp());
    }

    if let Some(seconds) = parse_relative(spec) {
        return Ok(now.timestamp() - seconds as i64);
    }

    if let Ok(ts) = DateTime::parse_from_rfc3339(spec) {
        return Ok(ts.timestamp());
    }

    if let Ok(secs) = spec.parse::<i64>() {
        if secs >= 0 {
            return Ok(secs);
        }
    }

    Err(DatadogError::InvalidTimeRange(spec.to_string()))
}

/// Parses a `(from, to)` pair. When `to` is `None`, defaults to `now`.
pub fn parse_time_range(from: &str, to: Option<&str>) -> Result<(i64, i64), DatadogError> {
    let now = Utc::now();
    let from_ts = parse_instant(from, now)?;
    let to_ts = match to {
        Some(spec) => parse_instant(spec, now)?,
        None => now.timestamp(),
    };
    if to_ts < from_ts {
        return Err(DatadogError::InvalidTimeRange(format!(
            "to ({to_ts}) is before from ({from_ts})"
        )));
    }
    Ok((from_ts, to_ts))
}

/// Parses `{N}{unit}` shorthand. Returns the number of seconds it represents,
/// or `None` if the input does not match the shorthand grammar.
fn parse_relative(spec: &str) -> Option<u64> {
    let unit_byte = *spec.as_bytes().last()?;
    let unit_seconds: u64 = match unit_byte {
        b's' => 1,
        b'm' => 60,
        b'h' => 60 * 60,
        b'd' => 24 * 60 * 60,
        b'w' => 7 * 24 * 60 * 60,
        _ => return None,
    };
    let digits = &spec[..spec.len() - 1];
    if digits.is_empty() {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(unit_seconds)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_now() {
        assert_eq!(parse_instant("now", now()).unwrap(), now().timestamp());
    }

    #[test]
    fn parses_now_case_insensitive() {
        assert_eq!(parse_instant("NOW", now()).unwrap(), now().timestamp());
        assert_eq!(parse_instant("Now", now()).unwrap(), now().timestamp());
    }

    #[test]
    fn parses_relative_minutes() {
        assert_eq!(
            parse_instant("15m", now()).unwrap(),
            now().timestamp() - 15 * 60
        );
    }

    #[test]
    fn parses_relative_seconds() {
        assert_eq!(parse_instant("30s", now()).unwrap(), now().timestamp() - 30);
    }

    #[test]
    fn parses_relative_hours() {
        assert_eq!(
            parse_instant("2h", now()).unwrap(),
            now().timestamp() - 2 * 60 * 60
        );
    }

    #[test]
    fn parses_relative_days() {
        assert_eq!(
            parse_instant("3d", now()).unwrap(),
            now().timestamp() - 3 * 24 * 60 * 60
        );
    }

    #[test]
    fn parses_relative_weeks() {
        assert_eq!(
            parse_instant("1w", now()).unwrap(),
            now().timestamp() - 7 * 24 * 60 * 60
        );
    }

    #[test]
    fn parses_rfc3339_utc() {
        let expected = Utc
            .with_ymd_and_hms(2026, 4, 22, 10, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            parse_instant("2026-04-22T10:00:00Z", now()).unwrap(),
            expected
        );
    }

    #[test]
    fn parses_rfc3339_with_offset() {
        let expected = Utc
            .with_ymd_and_hms(2026, 4, 22, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            parse_instant("2026-04-22T10:00:00+01:00", now()).unwrap(),
            expected
        );
    }

    #[test]
    fn parses_epoch_seconds() {
        assert_eq!(parse_instant("1700000000", now()).unwrap(), 1_700_000_000);
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_instant("", now()).is_err());
        assert!(parse_instant("   ", now()).is_err());
    }

    #[test]
    fn rejects_compound_relative() {
        assert!(parse_instant("1h30m", now()).is_err());
    }

    #[test]
    fn rejects_unit_without_digits() {
        assert!(parse_instant("h", now()).is_err());
        assert!(parse_instant("m", now()).is_err());
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(parse_instant("5y", now()).is_err());
    }

    #[test]
    fn rejects_negative_epoch() {
        assert!(parse_instant("-1", now()).is_err());
    }

    #[test]
    fn rejects_rfc3339_without_timezone() {
        assert!(parse_instant("2026-04-22T10:00:00", now()).is_err());
    }

    #[test]
    fn rejects_non_numeric_garbage() {
        assert!(parse_instant("zzz", now()).is_err());
    }

    #[test]
    fn time_range_defaults_to_to_now() {
        let (from, to) = parse_time_range("1h", None).unwrap();
        assert!(to - from <= 60 * 60 + 5);
        assert!(to - from >= 60 * 60 - 5);
    }

    #[test]
    fn time_range_explicit_to() {
        let (from, to) =
            parse_time_range("2026-04-22T09:00:00Z", Some("2026-04-22T10:00:00Z")).unwrap();
        assert_eq!(to - from, 60 * 60);
    }

    #[test]
    fn time_range_rejects_inverted_range() {
        let err =
            parse_time_range("2026-04-22T10:00:00Z", Some("2026-04-22T09:00:00Z")).unwrap_err();
        assert!(err.to_string().contains("before"));
    }

    #[test]
    fn time_range_propagates_from_error() {
        assert!(parse_time_range("garbage", None).is_err());
    }

    #[test]
    fn time_range_propagates_to_error() {
        assert!(parse_time_range("1h", Some("garbage")).is_err());
    }
}
