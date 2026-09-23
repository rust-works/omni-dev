//! Data-driven downstream effort advice. No provider execution configuration lives here.

use super::{Ladder, Stage, StageAnswer, NO_DESIGN};
use crate::jev::protocol::{Answer, Question};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const NOT_NEEDED: &str = "not_needed";
const INSUFFICIENT: &str = "insufficient";
const TEMPLATE: &str = include_str!("../../templates/jev-route-effort.txt");

/// A concrete model represented by a rung, optionally only for certain stages.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// Provider model identifier (opaque, including punctuation).
    pub name: String,
    /// Stages this binding covers; omitted means all stages.
    #[serde(default = "all_stages")]
    pub stages: Vec<Stage>,
    /// Official capability references, for maintenance rather than prompting.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Date the capability references were checked.
    pub verified: Option<String>,
    /// The model's native effort control and routing criteria.
    pub effort: Profile,
}

fn all_stages() -> Vec<Stage> {
    Stage::ALL.to_vec()
}

/// Native effort capabilities. Profiles are inline and can be shared with YAML anchors.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Profile {
    /// A caller can select one of the supported native levels.
    Configurable {
        /// Native parameter, informational only.
        control: String,
        /// Ordered vocabulary with routing criteria.
        levels: Vec<Level>,
        /// Levels actually supported by this model, in ascending effort order.
        supported_levels: Vec<String>,
    },
    /// A documented fixed setting; no judgement is required to select it.
    Fixed {
        /// Native control name.
        control: String,
        /// The sole setting.
        level: String,
        /// Why the setting is fixed.
        reason: String,
    },
    /// No verified, configurable control is available for this binding.
    Unavailable {
        /// Why advice cannot be offered.
        reason: String,
    },
}

/// One effort level's shared criterion with optional per-stage overrides.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Level {
    /// Native value, not a provider-neutral scale.
    pub name: String,
    /// Fallback criterion for stages without an override.
    pub description: Option<String>,
    /// Stage-specific criteria.
    #[serde(default)]
    pub criteria: BTreeMap<Stage, String>,
}

impl Level {
    fn criterion(&self, stage: Stage) -> Option<&str> {
        self.criteria
            .get(&stage)
            .map(String::as_str)
            .or(self.description.as_deref())
    }
}

/// Advice for each concrete model in one rung.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelEffort {
    /// Concrete model identifier, or the rung name for legacy unspecified data.
    pub model: String,
    /// Native control name when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
    /// Whether there is a recommendation or why there is none.
    pub status: Status,
    /// Native value, present only for recommended or fixed settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// Explanation for deterministic outcomes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Effort or remaining-work judgement, absent for legacy or normalized no-design results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assessment: Option<Assessment>,
    /// Effort uncertainty, separate from model-class close calls.
    pub close_call: bool,
}

/// The possible outcomes of downstream effort routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// A native effort level was selected.
    Recommended,
    /// No work remains for this stage.
    NotNeeded,
    /// Even maximum effort does not meet the reliability bar.
    Insufficient,
    /// The model has one documented setting.
    Fixed,
    /// No verified control is available.
    Unavailable,
    /// A legacy custom ladder has no effort metadata.
    Unspecified,
}

/// Jev's unmodified evidence for an effort or applicability judgement.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assessment {
    /// Selected level or outcome.
    pub choice: String,
    /// Jev confidence (not necessarily the chosen option's probability).
    pub confidence: f64,
    /// Probabilities of the offered options.
    pub probabilities: BTreeMap<String, f64>,
}

pub(super) fn validate(models: &[Model]) -> Result<()> {
    if models.is_empty() {
        bail!("models must not be empty; omit models for a legacy class-only rung");
    }
    let mut seen = BTreeSet::new();
    for model in models {
        nonempty(&model.name, "model name")?;
        if model.stages.is_empty() {
            bail!("model {:?} has no stages", model.name);
        }
        for &stage in &model.stages {
            if !seen.insert((&model.name, stage)) {
                bail!(
                    "model {:?} is defined more than once for {stage:?}",
                    model.name
                );
            }
        }
        validate_profile(model).with_context(|| format!("model {:?}", model.name))?;
    }
    for stage in Stage::ALL {
        if !models.iter().any(|m| m.stages.contains(&stage)) {
            bail!("no model binding for {stage:?}");
        }
    }
    Ok(())
}

fn nonempty(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} must be non-empty");
    }
    Ok(())
}

fn validate_profile(model: &Model) -> Result<()> {
    match &model.effort {
        Profile::Configurable {
            control,
            levels,
            supported_levels,
        } => {
            nonempty(control, "control")?;
            let mut names = BTreeSet::new();
            for level in levels {
                nonempty(&level.name, "level name")?;
                if [NOT_NEEDED, INSUFFICIENT].contains(&level.name.as_str()) {
                    bail!("level {:?} is a reserved outcome", level.name);
                }
                if !names.insert(&level.name) {
                    bail!("duplicate level {:?}", level.name);
                }
                if level
                    .description
                    .as_ref()
                    .is_some_and(|s| s.trim().is_empty())
                    || level.criteria.values().any(|s| s.trim().is_empty())
                {
                    bail!("level {:?} has empty criteria", level.name);
                }
                for &stage in &model.stages {
                    if level.criterion(stage).is_none() {
                        bail!("level {:?} has missing criteria for {stage:?}", level.name);
                    }
                }
            }
            if supported_levels.len() < 2 {
                bail!("configurable effort needs at least two supported levels; use kind: fixed for one");
            }
            let mut supported = BTreeSet::new();
            let mut previous_index = None;
            for name in supported_levels {
                let Some(index) = levels.iter().position(|level| &level.name == name) else {
                    bail!("unknown supported level {name:?}");
                };
                if !supported.insert(name) {
                    bail!("duplicate supported level {name:?}");
                }
                if previous_index.is_some_and(|previous| index < previous) {
                    bail!("supported levels must follow the order declared in levels");
                }
                previous_index = Some(index);
            }
        }
        Profile::Fixed {
            control,
            level,
            reason,
        } => {
            nonempty(control, "control")?;
            nonempty(level, "fixed level")?;
            if [NOT_NEEDED, INSUFFICIENT].contains(&level.as_str()) {
                bail!("fixed level {level:?} is a reserved outcome");
            }
            nonempty(reason, "fixed reason")?;
        }
        Profile::Unavailable { reason } => nonempty(reason, "unavailable reason")?,
    }
    Ok(())
}

fn key(ladder: &Ladder, stage: Stage, tier: usize, model: usize) -> String {
    format!(
        "{}.{}.effort_{tier}_{model}",
        ladder.name,
        stage.template_key()
    )
}

fn stage_context(stage: Stage) -> &'static str {
    match stage {
        Stage::Design => "Design: choose the approach, settle open questions, and write a plan.",
        Stage::Implement => "Implementation: write code, tests and docs. Assume remaining design has been completed well.",
        Stage::Review => "Review: review the finished change before merge for mistakes automated tests would miss.",
    }
}

fn question(model: &Model, description: &str, stage: Stage) -> Question {
    let instructions = format!(
        "{}\n{}\nModel: {}. Capability: {}",
        TEMPLATE.trim(),
        stage_context(stage),
        model.name,
        description
    );
    let mut criteria = BTreeMap::from([(
        NOT_NEEDED.to_string(),
        "No work remains for this stage; no effort is needed.".to_string(),
    )]);
    if let Profile::Configurable {
        control,
        levels,
        supported_levels,
    } = &model.effort
    {
        for name in supported_levels {
            if let Some(level) = levels.iter().find(|l| &l.name == name) {
                if let Some(criterion) = level.criterion(stage) {
                    criteria.insert(name.clone(), criterion.to_string());
                }
            }
        }
        criteria.insert(INSUFFICIENT.to_string(), "Work remains, but even this model's highest supported effort is unlikely to meet the reliability bar.".to_string());
        Question::Choice {
            instructions: format!(
                "{instructions}\nNative control: {control}. Levels from least to most effort: {}.",
                supported_levels.join(", ")
            ),
            criteria,
        }
    } else {
        criteria.insert(
            "remaining".to_string(),
            "Work remains for this stage.".to_string(),
        );
        Question::Choice { instructions: format!("{} Is there work remaining for this stage? Judge only the issue's remaining work, regardless of model capability.", stage_context(stage)), criteria }
    }
}

/// Non-configurable models share one stage-applicability question per ladder.
fn question_key(ladder: &Ladder, stage: Stage, tier: usize, index: usize, model: &Model) -> String {
    if matches!(model.effort, Profile::Configurable { .. }) {
        key(ladder, stage, tier, index)
    } else {
        format!("{}.{}.remaining", ladder.name, stage.template_key())
    }
}

pub(super) fn add_questions(ladder: &Ladder, questions: &mut BTreeMap<String, Question>) {
    for stage in Stage::ALL {
        for (tier_index, tier) in ladder.tiers.as_slice().iter().enumerate() {
            for (model_index, model) in tier.models.iter().flatten().enumerate() {
                if model.stages.contains(&stage) {
                    questions.insert(
                        question_key(ladder, stage, tier_index, model_index, model),
                        question(model, &tier.description, stage),
                    );
                }
            }
        }
    }
}

fn read(answers: &BTreeMap<String, Answer>, key: &str, question: &Question) -> Result<Assessment> {
    let Some(Answer::Choice {
        choice,
        confidence,
        probabilities,
    }) = answers.get(key)
    else {
        bail!("no choice answer for {key:?}");
    };
    let Question::Choice { criteria, .. } = question else {
        unreachable!()
    };
    if !criteria.contains_key(choice) {
        bail!("{key:?} chose unsupported effort/outcome {choice:?}");
    }
    if !confidence.is_finite() || !(0.0..=1.0).contains(confidence) {
        bail!("{key:?} has invalid confidence");
    }
    if probabilities.len() != criteria.len()
        || criteria.keys().any(|k| !probabilities.contains_key(k))
    {
        bail!("{key:?} probability keys must match offered levels/outcomes");
    }
    if probabilities
        .values()
        .any(|p| !p.is_finite() || !(0.0..=1.0).contains(p))
    {
        bail!("{key:?} has invalid probabilities");
    }
    Ok(Assessment {
        choice: choice.clone(),
        confidence: *confidence,
        probabilities: probabilities.clone(),
    })
}

pub(super) fn decode(
    ladder: &Ladder,
    stage: Stage,
    class: &StageAnswer,
    answers: &BTreeMap<String, Answer>,
    threshold: f64,
) -> Result<BTreeMap<String, Vec<ModelEffort>>> {
    let mut output = BTreeMap::new();
    for (ti, tier) in ladder.tiers.as_slice().iter().enumerate() {
        let mut entries = Vec::new();
        for (mi, model) in tier
            .models
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, m)| m.stages.contains(&stage))
        {
            let key = question_key(ladder, stage, ti, mi, model);
            let assessment = read(answers, &key, &question(model, &tier.description, stage))
                .with_context(|| {
                    format!(
                        "ladder {:?}, rung {:?}, model {:?}, {stage:?}",
                        ladder.name, tier.name, model.name
                    )
                })?;
            let (control, status, level, reason) = match &model.effort {
                Profile::Configurable { control, .. } => (
                    Some(control.clone()),
                    Status::Recommended,
                    Some(assessment.choice.clone()),
                    None,
                ),
                Profile::Fixed {
                    control,
                    level,
                    reason,
                } => (
                    Some(control.clone()),
                    Status::Fixed,
                    Some(level.clone()),
                    Some(reason.clone()),
                ),
                Profile::Unavailable { reason } => {
                    (None, Status::Unavailable, None, Some(reason.clone()))
                }
            };
            let mut entry = ModelEffort {
                model: model.name.clone(),
                control,
                status,
                level,
                reason,
                close_call: assessment.confidence < threshold,
                assessment: Some(assessment.clone()),
            };
            if assessment.choice == NOT_NEEDED
                || (stage == Stage::Design && class.choice == NO_DESIGN)
            {
                entry.status = Status::NotNeeded;
                entry.level = None;
                entry.reason = Some("No work remains for this stage.".to_string());
                if stage == Stage::Design && class.choice == NO_DESIGN {
                    entry.assessment = None;
                    entry.close_call = false;
                }
            } else if assessment.choice == INSUFFICIENT {
                entry.status = Status::Insufficient;
                entry.level = None;
                entry.reason = Some(
                    "No supported effort meets the reliability bar for this model.".to_string(),
                );
            }
            entries.push(entry);
        }
        if tier.models.is_none() {
            let no_design = stage == Stage::Design && class.choice == NO_DESIGN;
            entries.push(ModelEffort {
                model: tier.name.clone(),
                control: None,
                status: if no_design {
                    Status::NotNeeded
                } else {
                    Status::Unspecified
                },
                level: None,
                reason: Some(
                    if no_design {
                        "No design work remains."
                    } else {
                        "Legacy ladder: add models and effort profiles for effort advice."
                    }
                    .to_string(),
                ),
                assessment: None,
                close_call: false,
            });
        }
        output.insert(tier.name.clone(), entries);
    }
    Ok(output)
}

pub(super) fn render(
    stage: Stage,
    answers: &BTreeMap<String, Vec<ModelEffort>>,
    ladder: Option<&Ladder>,
) -> Vec<String> {
    let names: Vec<&str> = ladder.map_or_else(
        || answers.keys().map(String::as_str).collect(),
        |l| l.tiers.as_slice().iter().map(|t| t.name.as_str()).collect(),
    );
    let mut lines = Vec::new();
    for name in names {
        for entry in answers.get(name).into_iter().flatten() {
            let status = match entry.status {
                Status::Recommended | Status::Fixed => entry.level.as_deref().unwrap_or("unknown"),
                Status::NotNeeded => "not needed",
                Status::Insufficient => "insufficient capability",
                Status::Unavailable => "unavailable",
                Status::Unspecified => "unspecified (add effort metadata)",
            };
            let mut detail = status.to_string();
            if entry.status == Status::Fixed {
                detail.push_str(" (fixed)");
            }
            if entry.status == Status::Unavailable {
                if let Some(reason) = &entry.reason {
                    detail.push_str(&format!(" — {reason}"));
                }
            }
            if let Some(a) = &entry.assessment {
                let label = if matches!(entry.status, Status::Fixed | Status::Unavailable) {
                    "remaining work "
                } else {
                    ""
                };
                detail.push_str(&format!(" ({label}{:.2}", a.confidence));
                if entry.close_call {
                    detail.push_str(", close call");
                    if let Some((name, p)) = a
                        .probabilities
                        .iter()
                        .filter(|(k, _)| *k != &a.choice)
                        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
                    {
                        detail.push_str(&format!(" — {name} {p:.2}"));
                    }
                }
                detail.push(')');
            }
            let model = if entry.model == name {
                String::new()
            } else {
                format!(" [{}]", entry.model)
            };
            lines.push(format!(
                "    {} effort: {name}{model}: {detail}",
                stage.template_key().trim_start_matches("stage_")
            ));
        }
    }
    lines
}

#[cfg(test)]
#[path = "effort_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
