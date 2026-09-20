//! Sheets' relative-date keywords and the date-condition operand shape they
//! feed into.
//!
//! Shared by [`crate::drive::sheets::validation`] (`DATE_AFTER`/
//! `DATE_BEFORE`/`DATE_EQ`) and [`crate::drive::sheets::conditional_format`]
//! (`BooleanRule` conditions of the same shape) — extracted here, issue
//! #1793, since it's generic over both callers' condition enums rather than
//! coupled to either one.

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

#[cfg(test)]
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
}
