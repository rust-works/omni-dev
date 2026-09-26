//! Sheets' relative-date keywords, the date-condition operand shape they
//! feed into, and the condition-value validators both callers' `validate_*`
//! functions need.
//!
//! Shared by [`crate::drive::sheets::validation`] (`DATE_AFTER`/
//! `DATE_BEFORE`/`DATE_EQ`) and [`crate::drive::sheets::conditional_format`]
//! (`BooleanRule` conditions of the same shape) — extracted here, issue
//! #1793, since it's generic over both callers' condition enums rather than
//! coupled to either one. The `reject_*` validators below are the same
//! reasoning applied to the actual value-rejection logic: both files'
//! condition enums overlap on the numeric/text/date value shapes, and this
//! is where a fix to one (e.g. the NaN/reversed-range checks) reaches both
//! rather than only the file it was written in.

use crate::drive::sheets::types::ConditionValue;

/// One of Sheets' six `RelativeDate` values.
///
/// Usable wherever a date condition takes a single value
/// (`DATE_AFTER`/`DATE_BEFORE`/`DATE_EQ`); `DATE_BETWEEN` requires two
/// absolute dates and never accepts one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeDate {
    /// The 365 days up to and including today.
    PastYear,
    /// The 30 days up to and including today.
    PastMonth,
    /// The 7 days up to and including today.
    PastWeek,
    /// The day before today.
    Yesterday,
    /// Today.
    Today,
    /// The day after today.
    Tomorrow,
}

impl RelativeDate {
    /// Every variant, in no particular order. `parse` derives from this (via
    /// [`Self::as_sheets_str`]) rather than keeping its own hardcoded string
    /// table, so the two can't drift apart on a typo; the
    /// `relative_date_all_covers_every_variant` test's exhaustive match
    /// forces this list to grow alongside the enum.
    const ALL: [Self; 6] = [
        Self::PastYear,
        Self::PastMonth,
        Self::PastWeek,
        Self::Yesterday,
        Self::Today,
        Self::Tomorrow,
    ];

    pub(crate) const fn as_sheets_str(self) -> &'static str {
        match self {
            Self::PastYear => "PAST_YEAR",
            Self::PastMonth => "PAST_MONTH",
            Self::PastWeek => "PAST_WEEK",
            Self::Yesterday => "YESTERDAY",
            Self::Today => "TODAY",
            Self::Tomorrow => "TOMORROW",
        }
    }

    /// Matches case- and separator-insensitively (`past-week`, `past_week`,
    /// `Past Week` all match) so the CLI value doesn't force one style.
    ///
    /// Deriving this from [`Self::as_sheets_str`] via [`Self::ALL`] costs a
    /// per-candidate allocation (up to 6, plus one for `normalized`) instead
    /// of a single hand-written match — negligible for a one-shot CLI
    /// parse of 6 short constants, and deliberate: it's what keeps this
    /// keyword table and `as_sheets_str` from drifting apart on a typo (see
    /// `relative_date_all_covers_every_variant`). Don't "optimize" this back
    /// into a second hardcoded match.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let normalized = s.to_ascii_lowercase().replace(['-', '_', ' '], "");
        Self::ALL
            .into_iter()
            .find(|rd| rd.as_sheets_str().to_ascii_lowercase().replace('_', "") == normalized)
    }
}

/// A date condition's operand.
///
/// Either a literal date string (passed through untouched, the same
/// trust-the-caller stance as every other numeric/text value in this file —
/// Sheets parses it at evaluation time) or one of the six [`RelativeDate`]
/// keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateValue {
    /// A literal date string, untouched.
    Absolute(String),
    /// One of Sheets' relative-date keywords.
    Relative(RelativeDate),
}

impl DateValue {
    /// Never fails: anything that isn't a recognized relative keyword is
    /// treated as a literal date string.
    pub fn parse(raw: String) -> Self {
        match RelativeDate::parse(&raw) {
            Some(rd) => Self::Relative(rd),
            None => Self::Absolute(raw),
        }
    }

    pub(crate) fn is_blank(&self) -> bool {
        matches!(self, Self::Absolute(s) if s.trim().is_empty())
    }

    pub(crate) fn into_condition_value(self) -> ConditionValue {
        match self {
            Self::Absolute(s) => ConditionValue {
                user_entered_value: Some(s),
                relative_date: None,
            },
            Self::Relative(rd) => ConditionValue {
                user_entered_value: None,
                relative_date: Some(rd.as_sheets_str().to_string()),
            },
        }
    }
}

/// `NaN > x` and `x > NaN` are both `false`, so a NaN bound must be checked
/// explicitly or it silently reaches the API.
///
/// Shared by `validation.rs` and `conditional_format.rs` (issue #1793) — the
/// same numeric-range check both files' `--*-between` flags need.
pub(crate) fn reject_reversed_range(min: f64, max: f64, flag: &str) -> Result<(), String> {
    if min.is_nan() || max.is_nan() || min > max {
        Err(format!(
            "{flag}'s first value ({min}) must not exceed the second ({max})"
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn reject_nan(n: f64, flag: &str) -> Result<(), String> {
    if n.is_nan() {
        Err(format!("{flag} must not be NaN"))
    } else {
        Ok(())
    }
}

pub(crate) fn reject_blank(text: &str, flag: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        Err(format!("{flag} must not be empty"))
    } else {
        Ok(())
    }
}

/// Like [`reject_blank`], but only a zero-length string counts as empty — a
/// whitespace-only value is a meaningful thing to search for.
pub(crate) fn reject_empty(text: &str, flag: &str) -> Result<(), String> {
    if text.is_empty() {
        Err(format!("{flag} must not be empty"))
    } else {
        Ok(())
    }
}

pub(crate) fn reject_blank_date(date: &DateValue, flag: &str) -> Result<(), String> {
    if date.is_blank() {
        Err(format!("{flag} must not be empty"))
    } else {
        Ok(())
    }
}

/// `--date-between` takes two absolute dates only (see its own doc comment):
/// it never accepts a relative keyword, so one must be rejected loudly here rather than silently
/// reaching the API as a literal `userEnteredValue` string. Also mirrors
/// [`reject_reversed_range`]'s ordering check for the numeric family, on a
/// best-effort basis: a bound that doesn't parse as an ISO `YYYY-MM-DD` date
/// is trusted through untouched, the same trust-the-caller stance the rest
/// of this file takes for formats Sheets itself will parse at evaluation
/// time.
pub(crate) fn reject_invalid_date_between(start: &str, end: &str) -> Result<(), String> {
    reject_blank(start, "--date-between")?;
    reject_blank(end, "--date-between")?;
    if RelativeDate::parse(start).is_some() || RelativeDate::parse(end).is_some() {
        return Err(
            "--date-between only accepts absolute dates, not a relative keyword like 'today'"
                .to_string(),
        );
    }
    if let (Some(start_date), Some(end_date)) = (parse_iso_date(start), parse_iso_date(end)) {
        if start_date > end_date {
            return Err(format!(
                "--date-between's first value ({start}) must not be after the second ({end})"
            ));
        }
    }
    Ok(())
}

pub(crate) fn parse_iso_date(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn date_value_parse_is_case_and_separator_insensitive() {
        for input in ["past-week", "PAST_WEEK", "Past Week"] {
            assert!(matches!(
                DateValue::parse(input.to_string()),
                DateValue::Relative(RelativeDate::PastWeek)
            ));
        }
        assert!(matches!(
            DateValue::parse("2024-01-01".to_string()),
            DateValue::Absolute(s) if s == "2024-01-01"
        ));
    }

    #[test]
    fn relative_date_all_covers_every_variant() {
        // Exhaustive match, no `_` arm: adding a `RelativeDate` variant
        // without adding it to `RelativeDate::ALL` fails to compile here,
        // which is what forces `parse` (derived from `ALL`) to cover it too.
        fn assert_is_a_relative_date(rd: RelativeDate) {
            match rd {
                RelativeDate::PastYear
                | RelativeDate::PastMonth
                | RelativeDate::PastWeek
                | RelativeDate::Yesterday
                | RelativeDate::Today
                | RelativeDate::Tomorrow => {}
            }
        }
        for rd in RelativeDate::ALL {
            assert_is_a_relative_date(rd);
            assert_eq!(RelativeDate::parse(rd.as_sheets_str()), Some(rd));
        }
    }

    // ── shared validators (used by both `validation.rs` and
    // `conditional_format.rs`) ──────────────────────────────────────────

    #[test]
    fn reject_reversed_range_rejects_nan_and_reversed_bounds() {
        assert!(reject_reversed_range(1.0, 2.0, "--flag").is_ok());
        assert!(reject_reversed_range(2.0, 1.0, "--flag").is_err());
        assert!(reject_reversed_range(f64::NAN, 1.0, "--flag").is_err());
    }

    #[test]
    fn reject_invalid_date_between_rejects_a_relative_keyword_and_a_reversed_range() {
        assert!(reject_invalid_date_between("2024-01-01", "2024-01-02").is_ok());
        let err = reject_invalid_date_between("today", "2024-01-02").unwrap_err();
        assert!(err.contains("relative keyword"), "{err}");
        let err = reject_invalid_date_between("2024-01-02", "2024-01-01").unwrap_err();
        assert!(err.contains("must not be after"), "{err}");
    }

    #[test]
    fn parse_iso_date_accepts_only_the_yyyy_mm_dd_shape() {
        assert!(parse_iso_date("2024-01-01").is_some());
        assert!(parse_iso_date("not-a-date").is_none());
    }
}
