//! Wire types for TypeSafe's Jev "System One" REST API.
//!
//! The API answers a **map** of named questions about one opaque `state` in
//! a single request — `choice` (pick one of a labelled set), `score` (rate
//! against an ordered scale) and `noul` (a yes/no probability) all share the
//! one `/v1/systemone` endpoint, distinguished by an internally-tagged
//! `type` field. See `docs/jev.md` for the operator-facing guide.
//!
//! Every map here is a [`BTreeMap`], never a `HashMap`: neither `indexmap`
//! nor serde_json's `preserve_order` feature is a dependency, so a
//! `HashMap` would serialise in random order on every run.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::jev::error::JevError;

/// Default model identifier, used when no `--jev-model` / `TYPESAFE_MODEL`
/// override is configured.
pub const DEFAULT_MODEL: &str = "jev-latest";

/// A `POST /v1/systemone` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemOneRequest {
    /// The opaque program state the questions are asked about. Accepted as
    /// a JSON string, object, or array.
    pub state: serde_json::Value,

    /// The Jev model to use, e.g. [`DEFAULT_MODEL`].
    pub model: String,

    /// The questions to ask about `state`, keyed by caller-chosen name.
    /// Answered in one parallel pass; the response's `answers` map uses the
    /// same keys.
    pub questions: BTreeMap<String, Question>,
}

/// A single question asked of the `state`.
///
/// Internally tagged on `type` (`choice` | `score` | `noul`) — each variant's
/// shape, in particular whether `criteria` is a map, an ordered list, or
/// optional, differs by primitive and is not expressible as one flat struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Pick exactly one of a labelled set of options.
    Choice {
        /// Instructions describing what to choose and why.
        instructions: String,
        /// Option name to description, e.g. `{"billing": "Payment issues"}`.
        /// A `BTreeMap` alphabetises option order in the outgoing request —
        /// see `docs/jev.md` for the caveat this implies for `--option`.
        criteria: BTreeMap<String, String>,
    },

    /// Rate the state against an ordered scale.
    Score {
        /// Instructions describing what to rate and how.
        instructions: String,
        /// The scale's levels, low to high. Order is significant and is
        /// preserved (a `Vec`, not a map).
        criteria: Vec<String>,
    },

    /// Estimate the probability of a yes/no condition.
    Noul {
        /// Instructions describing the yes/no condition to estimate.
        instructions: String,
        /// Optional descriptions of what "true" and "false" mean. Omitted
        /// entirely from the request when `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

/// Minimum number of options a [`Question::Choice`] must offer — picking
/// "one of one" is not a judgment.
pub const MIN_CHOICE_OPTIONS: usize = 2;

/// Minimum number of levels a [`Question::Score`] scale must have.
pub const MIN_SCORE_LEVELS: usize = 2;

impl Question {
    /// Rejects degenerate questions: a `choice` needs at least
    /// [`MIN_CHOICE_OPTIONS`] options and a `score` at least
    /// [`MIN_SCORE_LEVELS`] levels. `noul` has none.
    ///
    /// This is omni-dev policy, not an API constraint: the live API accepts a
    /// one-option `choice` and a one-level `score` with `200`, but can only
    /// ever answer them with `confidence: 1.0`, so the paid call carries no
    /// information and almost always means a mistyped spec.
    ///
    /// Duplicate `choice` option names cannot be detected here — a
    /// [`BTreeMap`] has already collapsed them — so the `--option` parser
    /// checks those itself.
    pub fn validate(&self) -> Result<(), JevError> {
        match self {
            Self::Choice { criteria, .. } if criteria.len() < MIN_CHOICE_OPTIONS => {
                Err(JevError::InvalidQuestionSpec(format!(
                    "a choice question needs at least {MIN_CHOICE_OPTIONS} options, got {}",
                    criteria.len()
                )))
            }
            Self::Score { criteria, .. } if criteria.len() < MIN_SCORE_LEVELS => {
                Err(JevError::InvalidQuestionSpec(format!(
                    "a score question needs at least {MIN_SCORE_LEVELS} levels, got {}",
                    criteria.len()
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Optional descriptions of the two poles of a [`Question::Noul`] question.
///
/// Field names are the JSON string keys `"true"`/`"false"` (both Rust
/// keywords, hence the renames) rather than a nested `{value, description}`
/// shape, matching the verified wire format `{"true": "…", "false": "…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NoulCriteria {
    /// What a `true`-leaning answer means. Skipped when unset.
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    pub true_means: Option<String>,

    /// What a `false`-leaning answer means. Skipped when unset.
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    pub false_means: Option<String>,
}

/// A `POST /v1/systemone` response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemOneResponse {
    /// The concrete model version that produced the answers, e.g.
    /// `"jev-1.13.0"` (distinct from the request's `model`, which may be an
    /// alias like [`DEFAULT_MODEL`]).
    pub model: String,

    /// One answer per requested question, keyed identically to the
    /// request's `questions` map.
    pub answers: BTreeMap<String, Answer>,

    /// Token accounting for the request. Defaults to zero counts if the API
    /// ever omits the field.
    #[serde(default)]
    pub usage: Usage,
}

/// The answer to a single [`Question`].
///
/// Internally tagged on `type`, mirroring [`Question`]. Kept strict — no
/// catch-all variant — because `#[serde(other)]` only applies to unit
/// variants in an internally-tagged enum, so a new field on an existing
/// answer (which serde already ignores by default) is the free extension
/// path; a genuinely new `type` is a hard-error case that should name the
/// unrecognised value and suggest upgrading omni-dev, not silently swallow
/// it into a catch-all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    /// The chosen option, with per-option probabilities.
    Choice {
        /// The selected option's name (one of the request's `criteria` keys).
        choice: String,
        /// Model confidence in `choice`, in `[0, 1]`.
        confidence: f64,
        /// Probability assigned to every option that was offered.
        probabilities: BTreeMap<String, f64>,
    },

    /// The scored level, with a legend and per-level probabilities.
    Score {
        /// The estimated score, on the scale implied by the request's
        /// `criteria` (index-like, not necessarily an integer).
        score: f64,
        /// Model confidence in `score`, in `[0, 1]`.
        confidence: f64,
        /// Scale index (as a string key, e.g. `"0"`) to its label.
        legend: BTreeMap<String, String>,
        /// Probability assigned to every scale level.
        probabilities: BTreeMap<String, f64>,
    },

    /// The estimated yes/no probability.
    ///
    /// Deliberately has **no** `confidence` field — verified against a live
    /// response; unlike `choice`/`score`, `noul`'s single probability value
    /// already *is* its own confidence.
    Noul {
        /// Probability that the condition is true, in `[0, 1]`.
        noul: f64,
    },
}

/// Token accounting for one [`SystemOneRequest`]/[`SystemOneResponse`] pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Number of input tokens consumed (state + questions).
    pub input_tokens: u64,
    /// Number of output tokens produced (the answers).
    pub output_tokens: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── Question → wire JSON ────────────────────────────────────────────

    #[test]
    fn choice_question_serialises_with_map_criteria() {
        let question = Question::Choice {
            instructions: "Route this".to_string(),
            criteria: BTreeMap::from([
                ("billing".to_string(), "Payments".to_string()),
                ("technical".to_string(), "Bugs".to_string()),
            ]),
        };
        let value = serde_json::to_value(&question).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "choice",
                "instructions": "Route this",
                "criteria": {"billing": "Payments", "technical": "Bugs"}
            })
        );
    }

    #[test]
    fn score_question_serialises_with_ordered_array_criteria() {
        let question = Question::Score {
            instructions: "How urgent".to_string(),
            criteria: vec!["Low".to_string(), "Medium".to_string(), "High".to_string()],
        };
        let value = serde_json::to_value(&question).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "score",
                "instructions": "How urgent",
                "criteria": ["Low", "Medium", "High"]
            })
        );
    }

    #[test]
    fn noul_question_omits_criteria_when_none() {
        let question = Question::Noul {
            instructions: "Urgent?".to_string(),
            criteria: None,
        };
        let value = serde_json::to_value(&question).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "noul",
                "instructions": "Urgent?"
            })
        );
    }

    #[test]
    fn noul_question_serialises_criteria_as_true_false_map() {
        let question = Question::Noul {
            instructions: "Urgent?".to_string(),
            criteria: Some(NoulCriteria {
                true_means: Some("Customer is frustrated".to_string()),
                false_means: Some("Customer is calm".to_string()),
            }),
        };
        let value = serde_json::to_value(&question).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "noul",
                "instructions": "Urgent?",
                "criteria": {
                    "true": "Customer is frustrated",
                    "false": "Customer is calm"
                }
            })
        );
    }

    #[test]
    fn noul_criteria_serialises_partial_sides() {
        let criteria = NoulCriteria {
            true_means: Some("yes".to_string()),
            false_means: None,
        };
        let value = serde_json::to_value(&criteria).unwrap();
        assert_eq!(value, serde_json::json!({"true": "yes"}));
    }

    // ── Real verified live response ─────────────────────────────────────

    const LIVE_RESPONSE: &str = r#"{"model":"jev-1.13.0","answers":{"department":{"type":"choice","choice":"billing","confidence":0.97,"probabilities":{"sales":0.0,"technical":0.02,"billing":0.98}},"frustration":{"type":"score","score":1.01,"confidence":0.98,"legend":{"0":"Calm, just stating facts","1":"Frustrated but civil","2":"Very angry, strong language"},"probabilities":{"0":0.0,"1":0.99,"2":0.01}},"refund_requested":{"type":"noul","noul":0.98}},"usage":{"input_tokens":417,"output_tokens":71}}"#;

    #[test]
    fn live_response_deserialises() {
        let response: SystemOneResponse = serde_json::from_str(LIVE_RESPONSE).unwrap();
        assert_eq!(response.model, "jev-1.13.0");
        assert_eq!(response.usage.input_tokens, 417);
        assert_eq!(response.usage.output_tokens, 71);
        assert_eq!(response.answers.len(), 3);

        match &response.answers["department"] {
            Answer::Choice {
                choice,
                confidence,
                probabilities,
            } => {
                assert_eq!(choice, "billing");
                assert!((confidence - 0.97).abs() < f64::EPSILON);
                assert!((probabilities["billing"] - 0.98).abs() < f64::EPSILON);
            }
            other => panic!("expected Choice, got {other:?}"),
        }

        match &response.answers["frustration"] {
            Answer::Score {
                score,
                confidence,
                legend,
                probabilities,
            } => {
                assert!((score - 1.01).abs() < f64::EPSILON);
                assert!((confidence - 0.98).abs() < f64::EPSILON);
                assert_eq!(legend["0"], "Calm, just stating facts");
                assert!((probabilities["1"] - 0.99).abs() < f64::EPSILON);
            }
            other => panic!("expected Score, got {other:?}"),
        }
    }

    #[test]
    fn live_response_noul_answer_has_no_confidence_field() {
        let response: SystemOneResponse = serde_json::from_str(LIVE_RESPONSE).unwrap();
        match &response.answers["refund_requested"] {
            Answer::Noul { noul } => assert!((noul - 0.98).abs() < f64::EPSILON),
            other => panic!("expected Noul, got {other:?}"),
        }
    }

    #[test]
    fn unknown_extra_field_on_answer_is_ignored() {
        let json = serde_json::json!({
            "type": "noul",
            "noul": 0.5,
            "some_future_field": "unexpected"
        });
        let answer: Answer = serde_json::from_value(json).unwrap();
        assert_eq!(answer, Answer::Noul { noul: 0.5 });
    }

    #[test]
    fn usage_defaults_when_missing() {
        let json = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {}
        });
        let response: SystemOneResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.usage, Usage::default());
    }

    // ── Question::validate ──────────────────────────────────────────────

    fn choice_with(n: usize) -> Question {
        Question::Choice {
            instructions: "Route this".to_string(),
            criteria: (0..n)
                .map(|i| (format!("o{i}"), "desc".to_string()))
                .collect(),
        }
    }

    fn score_with(n: usize) -> Question {
        Question::Score {
            instructions: "How urgent".to_string(),
            criteria: (0..n).map(|i| format!("l{i}")).collect(),
        }
    }

    #[test]
    fn validate_rejects_choice_with_fewer_than_two_options() {
        for n in [0, 1] {
            let err = choice_with(n).validate().unwrap_err();
            assert!(matches!(err, JevError::InvalidQuestionSpec(_)));
            assert!(err.to_string().contains(&format!("got {n}")));
        }
        choice_with(2).validate().unwrap();
    }

    #[test]
    fn validate_rejects_score_with_fewer_than_two_levels() {
        for n in [0, 1] {
            let err = score_with(n).validate().unwrap_err();
            assert!(matches!(err, JevError::InvalidQuestionSpec(_)));
            assert!(err.to_string().contains(&format!("got {n}")));
        }
        score_with(2).validate().unwrap();
    }

    #[test]
    fn validate_accepts_noul_without_criteria() {
        Question::Noul {
            instructions: "Urgent?".to_string(),
            criteria: None,
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn default_model_constant() {
        assert_eq!(DEFAULT_MODEL, "jev-latest");
    }
}
