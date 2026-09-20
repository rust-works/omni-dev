//! Stage routing for `omni-dev ai jev route` (#1779, #1820).
//!
//! Asks Jev, in one `system_one` call per issue, which model class should do
//! each stage of the work — **design**, **implement** and **review** — and
//! derives the issue's overall class as the higher of design and implement.
//! The three questions are asked once per requested [`Ladder`] (one per AI
//! provider), keyed `<provider>.stage_<stage>`, all in that same single call.
//!
//! Everything here consumes only the provider-neutral [`IssueDoc`], so a
//! GitLab fetcher slots in without touching it. The question wording, the
//! `anthropic` tier descriptions and the input format are the ones validated
//! against `jev-1.13.0` in #1779; see `docs/jev.md` for the evidence and its
//! limits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::jev::citations::Citation;
use crate::jev::client::JevClient;
use crate::jev::error::is_auth_failure;
use crate::jev::input::truncate_middle;
use crate::jev::protocol::{Answer, Question, SystemOneRequest, Usage};
use crate::provider::{IssueDoc, ItemState};

pub use crate::jev::input::TRUNCATION_MARKER;

/// The three stage questions, each missing its per-tier criteria.
const STAGE_QUESTIONS_YAML: &str = include_str!("../templates/jev-route-questions.yaml");

/// The design-stage option meaning "no design work remains". Reserved: no
/// tier may use this name.
pub const NO_DESIGN: &str = "none";

/// The provider name a `--tiers FILE` ladder is reported under: a custom
/// file names no provider, and the output nests every ladder under one.
pub const CUSTOM_PROVIDER: &str = "custom";

/// An AI provider with an embedded model ladder (#1820).
///
/// Each variant's ladder lives in `src/templates/jev-route-tiers-<name>.yaml`,
/// three rungs, least capable first, named by the abbreviated model name so
/// the consumer gets a model to hand the work to rather than a rung to
/// translate. Only `anthropic`'s text was validated against Jev (#1779); the
/// other ladders reuse its descriptions rung for rung, differing only in
/// the tier names Jev sees as criterion keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Provider {
    /// `sonnet` / `opus` / `fable`.
    #[value(name = "anthropic")]
    Anthropic,
    /// `terra` / `sol` / `astra` (gpt-5.6-terra / gpt-5.6-sol / gpt-6-astra).
    #[value(name = "openai")]
    OpenAi,
    /// `flash` / `pro` / `deep-think` (gemini-3-flash-preview /
    /// gemini-3.1-pro-preview / Gemini 3 Deep Think).
    #[value(name = "gemini")]
    Gemini,
}

impl Provider {
    /// Every provider, in the order the docs list them.
    pub const ALL: [Self; 3] = [Self::Anthropic, Self::OpenAi, Self::Gemini];

    /// The provider's name: its clap value, its output key and its question
    /// key prefix.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
        }
    }

    /// The provider's embedded ladder file.
    const fn tiers_yaml(self) -> &'static str {
        match self {
            Self::Anthropic => include_str!("../templates/jev-route-tiers-anthropic.yaml"),
            Self::OpenAi => include_str!("../templates/jev-route-tiers-openai.yaml"),
            Self::Gemini => include_str!("../templates/jev-route-tiers-gemini.yaml"),
        }
    }

    /// The provider's embedded ladder, parsed and validated.
    pub fn tiers(self) -> Result<Tiers> {
        Tiers::parse(self.tiers_yaml())
            .with_context(|| format!("Invalid embedded tiers for provider {}", self.name()))
    }
}

/// Default confidence below which a stage is reported as a close call. A
/// working heuristic from the #1779 backlog run, not a validated threshold.
pub const DEFAULT_CLOSE_CALL: f64 = 0.3;

/// Default cap, in characters, on the issue text sent to Jev. Jev's input
/// limit is undocumented; the longest input tested was about 48k characters.
pub const DEFAULT_MAX_INPUT_CHARS: usize = 60_000;

/// Fewest tiers a tiers file may define — routing between one class is not
/// a judgment.
const MIN_TIERS: usize = 2;

/// The open citations found in each routed issue's own text (#1812).
///
/// Keyed by the citing issue's `(project, number)`. Built by the CLI's
/// blocking `gh` fetch (`fetch_docs`) — [`run_route`] only reads it, so it
/// stays free of GitHub-specific I/O.
pub type OpenDependencies = BTreeMap<(String, u64), Vec<Citation>>;

/// One stage of the work on an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Choosing the approach, settling open questions, writing a plan.
    Design,
    /// Writing the code, tests and docs.
    Implement,
    /// Reviewing the finished change before merge.
    Review,
}

impl Stage {
    /// Every stage, in the order they happen.
    const ALL: [Self; 3] = [Self::Design, Self::Implement, Self::Review];

    /// The key of this stage's question in the embedded questions file.
    const fn template_key(self) -> &'static str {
        match self {
            Self::Design => "stage_design",
            Self::Implement => "stage_implement",
            Self::Review => "stage_review",
        }
    }

    /// The key of this stage's question for `provider` in the Jev request
    /// and response, e.g. `anthropic.stage_design`.
    fn question_key(self, provider: &str) -> String {
        format!("{provider}.{}", self.template_key())
    }
}

/// One model class a stage can be routed to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    /// The option name Jev picks, e.g. `sonnet`.
    pub name: String,
    /// What this class is reliable at, shown to Jev as the option's criterion.
    pub description: String,
}

/// The on-disk shape of a tiers file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TiersFile {
    tiers: Vec<Tier>,
}

/// An ordered list of [`Tier`]s, from least to most capable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tiers(Vec<Tier>);

impl Tiers {
    /// Parses and validates a tiers file: at least two tiers, non-empty
    /// unique names and descriptions, and no tier named [`NO_DESIGN`].
    pub fn parse(yaml: &str) -> Result<Self> {
        let file: TiersFile = serde_yaml::from_str(yaml).context("Failed to parse tiers file")?;
        if file.tiers.len() < MIN_TIERS {
            bail!(
                "a tiers file needs at least {MIN_TIERS} tiers, got {}",
                file.tiers.len()
            );
        }
        let mut seen = BTreeSet::new();
        for tier in &file.tiers {
            if tier.name.trim().is_empty() || tier.description.trim().is_empty() {
                bail!("every tier needs a non-empty name and description");
            }
            if tier.name == NO_DESIGN {
                bail!("the tier name {NO_DESIGN:?} is reserved for \"no design work remains\"");
            }
            if !seen.insert(tier.name.as_str()) {
                bail!("tier {:?} is defined more than once", tier.name);
            }
        }
        Ok(Self(file.tiers))
    }

    /// Loads a custom tiers file.
    pub fn load_file(path: &Path) -> Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read tiers file {}", path.display()))?;
        Self::parse(&yaml).with_context(|| format!("Invalid tiers file {}", path.display()))
    }

    /// The tiers, least capable first.
    #[must_use]
    pub fn as_slice(&self) -> &[Tier] {
        &self.0
    }

    /// The rank of `choice` on the scale `none` (0) < first tier (1) < … —
    /// `None` if `choice` is neither [`NO_DESIGN`] nor a tier.
    fn rank(&self, choice: &str) -> Option<usize> {
        if choice == NO_DESIGN {
            return Some(0);
        }
        self.0.iter().position(|t| t.name == choice).map(|i| i + 1)
    }
}

/// One provider's model ladder: the name its answers are reported under and
/// the tiers Jev chooses between (#1820).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    /// The provider name: the output key and the question-key prefix.
    pub provider: String,
    /// The tiers, least capable first.
    pub tiers: Tiers,
}

impl Ladder {
    /// A built-in provider's embedded ladder.
    pub fn builtin(provider: Provider) -> Result<Self> {
        Ok(Self {
            provider: provider.name().to_string(),
            tiers: provider.tiers()?,
        })
    }

    /// A `--tiers FILE` ladder, reported under [`CUSTOM_PROVIDER`].
    #[must_use]
    pub fn custom(tiers: Tiers) -> Self {
        Self {
            provider: CUSTOM_PROVIDER.to_string(),
            tiers,
        }
    }
}

/// Builds the stage questions for every ladder: the embedded wording, with
/// one criterion per tier added, keyed `<provider>.stage_<stage>` so one
/// request carries every ladder's three questions.
pub fn build_route_questions(ladders: &[Ladder]) -> Result<BTreeMap<String, Question>> {
    let templates: BTreeMap<String, Question> = serde_yaml::from_str(STAGE_QUESTIONS_YAML)
        .context("Failed to parse the embedded stage questions")?;
    for stage in Stage::ALL {
        if !templates.contains_key(stage.template_key()) {
            bail!(
                "the embedded stage questions have no {:?}",
                stage.template_key()
            );
        }
    }
    let mut questions = BTreeMap::new();
    for ladder in ladders {
        for (key, template) in &templates {
            let mut question = template.clone();
            let Question::Choice { criteria, .. } = &mut question else {
                bail!("embedded stage question {key:?} is not a choice question");
            };
            for tier in ladder.tiers.as_slice() {
                if criteria
                    .insert(tier.name.clone(), tier.description.clone())
                    .is_some()
                {
                    bail!(
                        "tier {:?} of provider {:?} collides with a fixed option of {key:?}",
                        tier.name,
                        ladder.provider
                    );
                }
            }
            question
                .validate()
                .with_context(|| format!("stage question {key:?} for {:?}", ladder.provider))?;
            questions.insert(format!("{}.{key}", ladder.provider), question);
        }
    }
    Ok(questions)
}

/// Builds the text Jev judges for one issue: its title, body and human
/// comments. Returns the text and whether it was truncated.
///
/// Text over `max_chars` keeps its first and last halves with a
/// [`TRUNCATION_MARKER`] between them, rather than losing its end: the end
/// holds the latest comments, and a decision comment there is what moves the
/// design stage most (#1779). No validated input reached the cap, so this
/// policy itself is untested against Jev.
///
/// The layout is the input format validated in #1779 (condition C1), not the
/// slightly different format the issue body first proposed. The one
/// difference from the evaluation script is that bot comments are dropped
/// upstream, as the issue asks; the script kept them. Referenced issues and
/// pull requests are deliberately **not** included: they cost about twice the
/// tokens and inflate small issues.
#[must_use]
pub fn build_route_state(doc: &IssueDoc, max_chars: usize) -> (String, bool) {
    let mut text = format!("# #{} {}\n\n{}\n", doc.number, doc.title, doc.body.trim());
    if !doc.comments.is_empty() {
        text.push_str("\n\n---\n\n## Comments on this issue\n");
        for comment in &doc.comments {
            text.push_str(&format!(
                "\n\n**Comment by {}:**\n\n{}\n",
                comment.author,
                comment.body.trim()
            ));
        }
    }
    truncate_middle(&text, max_chars)
}

/// Jev's answer for one stage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageAnswer {
    /// The chosen tier, or [`NO_DESIGN`] for the design stage.
    pub choice: String,
    /// Jev's confidence in `choice`.
    pub confidence: f64,
    /// The probability of every option offered.
    pub probabilities: BTreeMap<String, f64>,
}

/// The answers for all three stages of one issue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageAnswers {
    /// Who should do the remaining design work, if any.
    pub design: StageAnswer,
    /// Who should write the code, tests and docs.
    pub implement: StageAnswer,
    /// Who should review the finished change.
    pub review: StageAnswer,
}

impl StageAnswers {
    fn get(&self, stage: Stage) -> &StageAnswer {
        match stage {
            Stage::Design => &self.design,
            Stage::Implement => &self.implement,
            Stage::Review => &self.review,
        }
    }
}

/// One open issue/PR a routed issue's text cites, and whether resolving it
/// would plausibly reduce the remaining work (#1812).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DependencyEntry {
    /// The citation exactly as written, e.g. `"#1129"` or `"PR #1629"` — not
    /// a reconstructed `owner/repo#N`, since the raw text already carries
    /// whatever form the issue used.
    #[serde(rename = "ref")]
    pub item_ref: String,
    /// Always [`ItemState::Open`]: a closed citation is settled and is not
    /// reported here.
    pub state: ItemState,
    /// The probability, per stage, that resolving this citation would leave
    /// less of that stage's work remaining than the text implies. Only
    /// `"design"` is populated for v1 — `"implement"` is deferred pending
    /// the same kind of live validation `design` got (see #1812).
    pub could_be_cheaper: BTreeMap<String, f64>,
}

/// The routing result for one issue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IssueRoute {
    /// The issue, as `owner/repo#N`.
    #[serde(rename = "ref")]
    pub item_ref: String,
    /// The issue's web URL.
    pub url: String,
    /// The issue title.
    pub title: String,
    /// The routing, or why this issue could not be routed.
    #[serde(flatten)]
    pub outcome: RouteOutcome,
    /// Whether the issue text was cut at the input cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// One provider's routing of an issue (#1820).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderRoute {
    /// Jev's answer per stage.
    pub stages: StageAnswers,
    /// The issue's class: the higher of the design and implement choices.
    pub class: String,
    /// Stages whose confidence is below the close-call threshold.
    pub close_calls: Vec<Stage>,
}

/// What routing one issue produced. Serialised untagged and flattened into
/// [`IssueRoute`], so a routed issue carries `providers`/`depends_on` and a
/// failed one carries only `error`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RouteOutcome {
    /// Jev answered every stage for every ladder.
    Routed {
        /// Each requested ladder's routing, keyed by provider name.
        providers: BTreeMap<String, ProviderRoute>,
        /// Open issues/PRs this issue's text cites (#1812). About the issue,
        /// not a provider, so it sits beside `providers` rather than inside.
        depends_on: Vec<DependencyEntry>,
    },
    /// The Jev call for this issue failed, or its answer was unusable. The
    /// other issues are still routed, so a long run keeps what it paid for.
    Failed {
        /// The error chain, on one line.
        error: String,
    },
}

impl IssueRoute {
    /// Whether this issue failed to route.
    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(self.outcome, RouteOutcome::Failed { .. })
    }
}

/// The routing result for every issue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteReport {
    /// The Jev model that answered (comma-separated in the unlikely case the
    /// alias moved mid-run).
    pub model: String,
    /// One entry per issue, in the order requested.
    pub issues: Vec<IssueRoute>,
    /// Token usage summed over every Jev call.
    pub usage: Usage,
}

/// Knobs for [`run_route`].
#[derive(Debug, Clone)]
pub struct RouteOptions {
    /// The Jev model to request.
    pub model: String,
    /// Confidence below which a stage is a close call.
    pub close_call: f64,
    /// Cap, in characters, on each issue's text.
    pub max_input_chars: usize,
    /// Route closed issues instead of refusing them.
    pub allow_closed: bool,
}

/// Routes every issue in `docs`, one Jev call each, in order.
///
/// Every ladder in `ladders` is routed in that same call: Jev takes a map of
/// questions over one state, so the three stage questions are keyed per
/// provider and share the issue text (#1820).
///
/// Refuses closed issues unless [`RouteOptions::allow_closed`]: their comments
/// often describe how the work was actually done, which leaks the answer.
///
/// `dependencies` is each issue's open citations (#1812), keyed by `(project,
/// number)` — pre-resolved by the caller's `gh` fetch, since this function
/// only ever talks to Jev. An issue with no entry (or an empty one) gets an
/// empty `depends_on` and no extra questions.
///
/// A failure on one issue is recorded on that issue as
/// [`RouteOutcome::Failed`] and the run continues, so a transient error late
/// in a long `--all-open` run does not discard the answers already paid for.
/// The exception is an authentication failure (HTTP 401/403), which would
/// fail every remaining issue the same way and so ends the run.
pub async fn run_route(
    jev: &JevClient,
    docs: &[IssueDoc],
    ladders: &[Ladder],
    opts: &RouteOptions,
    dependencies: &OpenDependencies,
) -> Result<RouteReport> {
    validate_options(docs, ladders, opts)?;
    let questions = build_route_questions(ladders)?;

    let mut models = BTreeSet::new();
    let mut usage = Usage::default();
    let mut issues = Vec::with_capacity(docs.len());
    for doc in docs {
        let item_ref = format!("{}#{}", doc.project, doc.number);
        let (state, truncated) = build_route_state(doc, opts.max_input_chars);
        if truncated {
            warn!(
                "Issue {item_ref} is longer than {} characters; its input was truncated",
                opts.max_input_chars
            );
        }
        let citations = dependencies
            .get(&(doc.project.clone(), doc.number))
            .map_or(&[][..], Vec::as_slice);
        let mut request_questions = questions.clone();
        for (i, citation) in citations.iter().enumerate() {
            request_questions.insert(
                could_be_cheaper_key(i),
                could_be_cheaper_question(&citation.raw),
            );
        }
        let request = SystemOneRequest {
            state: serde_json::Value::String(state),
            model: opts.model.clone(),
            questions: request_questions,
        };
        let outcome = match jev.system_one(&request).await {
            Err(err) if is_auth_failure(&err) => {
                return Err(err).with_context(|| format!("Failed to route issue {item_ref}"));
            }
            Err(err) => failed(&item_ref, &err.context("Jev request failed")),
            Ok(response) => {
                models.insert(response.model);
                usage.input_tokens += response.usage.input_tokens;
                usage.output_tokens += response.usage.output_tokens;
                match provider_routes(&response.answers, ladders, opts.close_call) {
                    Ok(providers) => RouteOutcome::Routed {
                        providers,
                        depends_on: dependency_entries(&item_ref, citations, &response.answers),
                    },
                    Err(err) => failed(&item_ref, &err.context("Unexpected Jev answer")),
                }
            }
        };
        issues.push(IssueRoute {
            item_ref,
            url: doc.url.clone(),
            title: doc.title.clone(),
            outcome,
            truncated,
        });
    }

    Ok(RouteReport {
        model: models.into_iter().collect::<Vec<_>>().join(", "),
        issues,
        usage,
    })
}

/// Records `err` as `item_ref`'s outcome, warning so a long run shows it as
/// it happens.
fn failed(item_ref: &str, err: &anyhow::Error) -> RouteOutcome {
    let error = format!("{err:#}");
    warn!("Could not route issue {item_ref}: {error}");
    RouteOutcome::Failed { error }
}

/// Rejects an empty run, an empty or repeated ladder list, out-of-range
/// knobs, and (by default) closed issues, before any paid request is sent.
fn validate_options(docs: &[IssueDoc], ladders: &[Ladder], opts: &RouteOptions) -> Result<()> {
    if docs.is_empty() {
        bail!("no issues to route");
    }
    if ladders.is_empty() {
        bail!("no providers to route against");
    }
    let mut seen = BTreeSet::new();
    for ladder in ladders {
        if !seen.insert(ladder.provider.as_str()) {
            bail!("provider {:?} is listed more than once", ladder.provider);
        }
    }
    if !(0.0..=1.0).contains(&opts.close_call) {
        bail!(
            "the close-call threshold must be between 0 and 1, got {}",
            opts.close_call
        );
    }
    if opts.max_input_chars == 0 {
        bail!("the input cap must be at least 1 character");
    }
    if !opts.allow_closed {
        let closed: Vec<String> = docs
            .iter()
            .filter(|d| d.state == ItemState::Closed)
            .map(|d| format!("{}#{}", d.project, d.number))
            .collect();
        if !closed.is_empty() {
            bail!(
                "refusing to route closed issues ({}): their comments often describe how the \
                 work was done, which leaks the answer. Pass --allow-closed to route them anyway",
                closed.join(", ")
            );
        }
    }
    Ok(())
}

/// Reads every ladder's routing out of one response. Any ladder whose
/// answers are missing or malformed fails the whole issue, as a single
/// ladder's did before #1820.
fn provider_routes(
    answers: &BTreeMap<String, Answer>,
    ladders: &[Ladder],
    close_call: f64,
) -> Result<BTreeMap<String, ProviderRoute>> {
    ladders
        .iter()
        .map(|ladder| {
            let stages = stage_answers(answers, ladder)?;
            let route = ProviderRoute {
                class: issue_class(&stages, &ladder.tiers),
                close_calls: close_calls(&stages, close_call),
                stages,
            };
            Ok((ladder.provider.clone(), route))
        })
        .collect()
}

/// Reads one ladder's three stage answers out of a response, checking each
/// is a `choice` naming an option that was offered.
fn stage_answers(answers: &BTreeMap<String, Answer>, ladder: &Ladder) -> Result<StageAnswers> {
    let tiers = &ladder.tiers;
    let read = |stage: Stage| -> Result<StageAnswer> {
        let key = stage.question_key(&ladder.provider);
        let Some(Answer::Choice {
            choice,
            confidence,
            probabilities,
        }) = answers.get(&key)
        else {
            bail!("no choice answer for {key:?}");
        };
        let offered = match tiers.rank(choice) {
            Some(0) => stage == Stage::Design,
            Some(_) => true,
            None => false,
        };
        if !offered {
            bail!("{key:?} chose {choice:?}, which was not offered");
        }
        Ok(StageAnswer {
            choice: choice.clone(),
            confidence: *confidence,
            probabilities: probabilities.clone(),
        })
    };
    Ok(StageAnswers {
        design: read(Stage::Design)?,
        implement: read(Stage::Implement)?,
        review: read(Stage::Review)?,
    })
}

/// The higher of the design and implement choices. Implement is always a
/// tier, so the result is always a tier, never [`NO_DESIGN`].
fn issue_class(stages: &StageAnswers, tiers: &Tiers) -> String {
    [&stages.design, &stages.implement]
        .into_iter()
        .max_by_key(|a| tiers.rank(&a.choice).unwrap_or_default())
        .map(|a| a.choice.clone())
        .unwrap_or_default()
}

/// The stages whose confidence is below `threshold`, in stage order.
fn close_calls(stages: &StageAnswers, threshold: f64) -> Vec<Stage> {
    Stage::ALL
        .into_iter()
        .filter(|&s| stages.get(s).confidence < threshold)
        .collect()
}

/// The request key for the `i`th open citation's `could_be_cheaper` question.
fn could_be_cheaper_key(i: usize) -> String {
    format!("could_be_cheaper_{i}")
}

/// Builds the `could_be_cheaper` question for one open citation (#1812).
/// Design-stage only for v1 — the exact wording validated live against
/// `jev-1.13.0`, 2026-09-20; see docs/jev.md for the evidence. Do not reword
/// without re-validating (pinned by
/// `could_be_cheaper_question_is_the_tested_wording`).
fn could_be_cheaper_question(citation: &str) -> Question {
    Question::Noul {
        instructions: format!(
            "This issue cites {citation}, which is still open. If {citation} is resolved, how \
             likely is it that LESS design work would remain for THIS issue than the current \
             text implies — as opposed to this issue's own remaining work being unaffected, \
             because it is already scoped separately, is a parallel/sibling effort, or \
             {citation} is otherwise not a precondition for finishing this issue's own \
             remaining work?"
        ),
        criteria: None,
    }
}

/// Builds `depends_on` from `citations` and the Jev response that answered
/// one `could_be_cheaper` question per citation.
///
/// A missing or malformed answer for one citation drops just that
/// dependency — logged, not failed — since `could_be_cheaper` is additive on
/// top of a routing result ([`stage_answers`]) that already succeeded.
fn dependency_entries(
    item_ref: &str,
    citations: &[Citation],
    answers: &BTreeMap<String, Answer>,
) -> Vec<DependencyEntry> {
    citations
        .iter()
        .enumerate()
        .filter_map(|(i, citation)| {
            if let Some(Answer::Noul { noul }) = answers.get(&could_be_cheaper_key(i)) {
                return Some(DependencyEntry {
                    item_ref: citation.raw.clone(),
                    state: ItemState::Open,
                    could_be_cheaper: BTreeMap::from([("design".to_string(), *noul)]),
                });
            }
            warn!(
                "Issue {item_ref}: no could_be_cheaper answer for citation {:?}",
                citation.raw
            );
            None
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::provider::{Comment, GitProvider, ItemKind, ItemRef};

    fn doc(number: u64, state: ItemState) -> IssueDoc {
        IssueDoc {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            number,
            kind: ItemKind::Issue,
            title: "Route issues".to_string(),
            state,
            body: "  The body.\n".to_string(),
            comments: vec![],
            closed_by: vec![],
            url: format!("https://github.com/rust-works/omni-dev/issues/{number}"),
        }
    }

    fn opts() -> RouteOptions {
        RouteOptions {
            model: "jev-latest".to_string(),
            close_call: DEFAULT_CLOSE_CALL,
            max_input_chars: DEFAULT_MAX_INPUT_CHARS,
            allow_closed: false,
        }
    }

    fn default_tiers() -> Tiers {
        Provider::Anthropic.tiers().unwrap()
    }

    fn anthropic() -> Vec<Ladder> {
        vec![Ladder::builtin(Provider::Anthropic).unwrap()]
    }

    fn tier_names(tiers: &Tiers) -> Vec<&str> {
        tiers.as_slice().iter().map(|t| t.name.as_str()).collect()
    }

    // ── Tiers and providers ──────────────────────────────────────────

    #[test]
    fn anthropic_tiers_are_sonnet_opus_fable_in_order() {
        assert_eq!(tier_names(&default_tiers()), ["sonnet", "opus", "fable"]);
    }

    #[test]
    fn anthropic_tier_descriptions_are_the_tested_text() {
        let tiers = default_tiers();
        let fable = &tiers.as_slice()[2];
        assert_eq!(
            fable.description,
            "Strongest at open-ended design and research: choosing between architectures, \
             working in unfamiliar territory with no precedent in the codebase, anticipating \
             failure modes, and security-critical judgement."
        );
    }

    #[test]
    fn provider_names_are_the_documented_abbreviations() {
        assert_eq!(
            tier_names(&Provider::OpenAi.tiers().unwrap()),
            ["terra", "sol", "astra"]
        );
        assert_eq!(
            tier_names(&Provider::Gemini.tiers().unwrap()),
            ["flash", "pro", "deep-think"]
        );
        assert_eq!(
            Provider::ALL.map(Provider::name),
            ["anthropic", "openai", "gemini"]
        );
        for provider in Provider::ALL {
            assert_eq!(
                provider.to_possible_value().unwrap().get_name(),
                provider.name()
            );
        }
    }

    /// Only the anthropic text was validated (#1779), and rewording shifts
    /// answers, so the other ladders copy it rung for rung. A ladder that
    /// diverges must do so deliberately, by editing this test too.
    #[test]
    fn openai_and_gemini_ladders_reuse_the_anthropic_descriptions_rung_for_rung() {
        let anthropic = default_tiers();
        for provider in [Provider::OpenAi, Provider::Gemini] {
            let tiers = provider.tiers().unwrap();
            assert_eq!(tiers.as_slice().len(), anthropic.as_slice().len());
            for (rung, reference) in tiers.as_slice().iter().zip(anthropic.as_slice()) {
                assert_eq!(
                    rung.description,
                    reference.description,
                    "{}: {} differs from {}",
                    provider.name(),
                    rung.name,
                    reference.name
                );
            }
        }
    }

    #[test]
    fn ladders_are_named_by_provider_or_custom() {
        let ladder = Ladder::builtin(Provider::Gemini).unwrap();
        assert_eq!(ladder.provider, "gemini");
        let custom = Ladder::custom(default_tiers());
        assert_eq!(custom.provider, CUSTOM_PROVIDER);
        assert_eq!(custom.tiers, default_tiers());
    }

    #[test]
    fn tiers_need_at_least_two() {
        let err = Tiers::parse("tiers:\n  - {name: a, description: A}\n").unwrap_err();
        assert!(err.to_string().contains("at least 2 tiers"), "{err}");
    }

    #[test]
    fn tiers_reject_the_reserved_none() {
        let err = Tiers::parse(
            "tiers:\n  - {name: a, description: A}\n  - {name: none, description: B}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn tiers_reject_duplicate_names() {
        let err =
            Tiers::parse("tiers:\n  - {name: a, description: A}\n  - {name: a, description: B}\n")
                .unwrap_err();
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[test]
    fn tiers_reject_empty_descriptions() {
        let err =
            Tiers::parse("tiers:\n  - {name: a, description: A}\n  - {name: b, description: ''}\n")
                .unwrap_err();
        assert!(err.to_string().contains("non-empty"), "{err}");
    }

    #[test]
    fn tiers_reject_unknown_fields() {
        let err = Tiers::parse(
            "tiers:\n  - {name: a, description: A, rank: 1}\n  - {name: b, description: B}\n",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
    }

    #[test]
    fn tiers_load_reads_a_custom_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiers.yaml");
        std::fs::write(
            &path,
            "tiers:\n  - {name: small, description: S}\n  - {name: big, description: B}\n",
        )
        .unwrap();
        let tiers = Tiers::load_file(&path).unwrap();
        assert_eq!(tiers.as_slice()[1].name, "big");
    }

    #[test]
    fn tiers_load_names_a_missing_file() {
        let err = Tiers::load_file(Path::new("/no/such/tiers.yaml")).unwrap_err();
        assert!(err.to_string().contains("Failed to read tiers file"));
    }

    // ── questions ────────────────────────────────────────────────────

    const BAR: &str = " Choose the least capable class likely to complete this stage correctly \
                       with no rework, about 9 times in 10. Judge the work that remains given \
                       the text, not the size of the text.";

    fn instructions(questions: &BTreeMap<String, Question>, key: &str) -> String {
        let Question::Choice { instructions, .. } = &questions[key] else {
            panic!("{key} is not a choice");
        };
        instructions.clone()
    }

    fn options(questions: &BTreeMap<String, Question>, key: &str) -> Vec<String> {
        let Question::Choice { criteria, .. } = &questions[key] else {
            panic!("{key} is not a choice");
        };
        criteria.keys().cloned().collect()
    }

    /// Pins the tested wording: changing it shifts every answer, so an edit
    /// to the template must also be a deliberate edit here.
    #[test]
    fn stage_questions_are_the_tested_wording() {
        let questions = build_route_questions(&anthropic()).unwrap();
        assert_eq!(
            instructions(&questions, "anthropic.stage_design"),
            format!(
                "Which class should do the design work that remains before implementation can \
                 start: choosing the approach, settling open questions, and writing a plan?{BAR}"
            )
        );
        assert_eq!(
            instructions(&questions, "anthropic.stage_implement"),
            format!(
                "Assume any remaining design work has been completed well. Which class should \
                 write the code, tests and docs?{BAR}"
            )
        );
        assert_eq!(
            instructions(&questions, "anthropic.stage_review"),
            format!(
                "Which class should review the finished change before merge, so that mistakes \
                 the automated tests would miss are caught?{BAR}"
            )
        );
    }

    #[test]
    fn only_the_design_stage_offers_none() {
        let questions = build_route_questions(&anthropic()).unwrap();
        assert_eq!(
            options(&questions, "anthropic.stage_design"),
            ["fable", "none", "opus", "sonnet"]
        );
        assert_eq!(
            options(&questions, "anthropic.stage_implement"),
            ["fable", "opus", "sonnet"]
        );
        assert_eq!(
            options(&questions, "anthropic.stage_review"),
            ["fable", "opus", "sonnet"]
        );
        let Question::Choice { criteria, .. } = &questions["anthropic.stage_design"] else {
            panic!();
        };
        assert_eq!(
            criteria[NO_DESIGN],
            "No design work remains: the text already settles the approach and the open \
             questions."
        );
    }

    /// Several ladders share one request: the same wording under each
    /// provider's prefix, with that provider's tiers as the options.
    #[test]
    fn questions_are_keyed_per_provider_in_one_map() {
        let ladders = [
            Ladder::builtin(Provider::Anthropic).unwrap(),
            Ladder::builtin(Provider::OpenAi).unwrap(),
        ];
        let questions = build_route_questions(&ladders).unwrap();
        let keys: Vec<&String> = questions.keys().collect();
        assert_eq!(
            keys,
            [
                "anthropic.stage_design",
                "anthropic.stage_implement",
                "anthropic.stage_review",
                "openai.stage_design",
                "openai.stage_implement",
                "openai.stage_review",
            ]
        );
        assert_eq!(
            options(&questions, "openai.stage_design"),
            ["astra", "none", "sol", "terra"]
        );
        assert_eq!(
            instructions(&questions, "openai.stage_review"),
            instructions(&questions, "anthropic.stage_review")
        );
    }

    #[test]
    fn a_custom_ladder_is_keyed_custom() {
        let ladders = [Ladder::custom(
            Tiers::parse("tiers:\n  - {name: a, description: A}\n  - {name: b, description: B}\n")
                .unwrap(),
        )];
        let questions = build_route_questions(&ladders).unwrap();
        assert_eq!(options(&questions, "custom.stage_implement"), ["a", "b"]);
    }

    // ── build_route_state ────────────────────────────────────────────

    #[test]
    fn route_state_without_comments_is_title_and_trimmed_body() {
        let (state, truncated) = build_route_state(&doc(7, ItemState::Open), 1000);
        assert_eq!(state, "# #7 Route issues\n\nThe body.\n");
        assert!(!truncated);
    }

    #[test]
    fn route_state_lists_comments_in_order_in_the_tested_format() {
        let mut d = doc(7, ItemState::Open);
        d.comments = vec![
            Comment {
                author: "alice".to_string(),
                body: "first\n".to_string(),
                id: None,
            },
            Comment {
                author: "bob".to_string(),
                body: "second".to_string(),
                id: None,
            },
        ];
        let (state, _) = build_route_state(&d, 1000);
        assert_eq!(
            state,
            "# #7 Route issues\n\nThe body.\n\n\n---\n\n## Comments on this issue\n\
             \n\n**Comment by alice:**\n\nfirst\n\
             \n\n**Comment by bob:**\n\nsecond\n"
        );
    }

    #[test]
    fn route_state_truncates_the_middle_on_a_char_boundary() {
        let mut d = doc(7, ItemState::Open);
        d.body = "é".repeat(100);
        let (full, _) = build_route_state(&d, usize::MAX);
        let (state, truncated) = build_route_state(&d, 20);
        assert!(truncated);
        let (head, tail) = state
            .split_once(&format!("\n\n{TRUNCATION_MARKER}\n\n"))
            .unwrap();
        assert_eq!(head.chars().count(), 10);
        assert_eq!(tail.chars().count(), 10);
        assert!(full.starts_with(head));
        assert!(full.ends_with(tail));
    }

    /// The latest comment is where a decision lands, so a cut must not lose it.
    #[test]
    fn route_state_truncation_keeps_the_latest_comment() {
        let mut d = doc(7, ItemState::Open);
        d.body = "x".repeat(10_000);
        d.comments = vec![Comment {
            author: "maintainer".to_string(),
            body: "**Decision**: settled.".to_string(),
            id: None,
        }];
        let (state, truncated) = build_route_state(&d, 1_000);
        assert!(truncated);
        assert!(state.starts_with("# #7 Route issues"));
        assert!(state.ends_with("**Decision**: settled.\n"), "{state}");
    }

    #[test]
    fn route_state_truncation_handles_a_one_char_cap() {
        let (state, truncated) = build_route_state(&doc(7, ItemState::Open), 1);
        assert!(truncated);
        assert_eq!(state, format!("\n\n{TRUNCATION_MARKER}\n\n\n"));
    }

    #[test]
    fn route_state_at_exactly_the_cap_is_not_truncated() {
        let d = doc(7, ItemState::Open);
        let (full, _) = build_route_state(&d, usize::MAX);
        let (state, truncated) = build_route_state(&d, full.chars().count());
        assert_eq!(state, full);
        assert!(!truncated);
    }

    // ── class and close calls ────────────────────────────────────────

    fn answer(choice: &str, confidence: f64) -> StageAnswer {
        StageAnswer {
            choice: choice.to_string(),
            confidence,
            probabilities: BTreeMap::new(),
        }
    }

    fn stages(design: &str, implement: &str, review: &str) -> StageAnswers {
        StageAnswers {
            design: answer(design, 0.9),
            implement: answer(implement, 0.9),
            review: answer(review, 0.9),
        }
    }

    #[test]
    fn class_is_the_higher_of_design_and_implement() {
        let tiers = default_tiers();
        assert_eq!(
            issue_class(&stages("fable", "sonnet", "sonnet"), &tiers),
            "fable"
        );
        assert_eq!(
            issue_class(&stages("sonnet", "opus", "sonnet"), &tiers),
            "opus"
        );
    }

    #[test]
    fn class_ignores_review_and_none() {
        let tiers = default_tiers();
        assert_eq!(
            issue_class(&stages("none", "sonnet", "fable"), &tiers),
            "sonnet"
        );
    }

    #[test]
    fn close_calls_are_below_the_threshold_in_stage_order() {
        let mut s = stages("fable", "sonnet", "opus");
        s.review.confidence = 0.1;
        s.design.confidence = 0.29;
        s.implement.confidence = 0.3;
        assert_eq!(close_calls(&s, 0.3), [Stage::Design, Stage::Review]);
    }

    #[test]
    fn could_be_cheaper_key_is_stable_and_indexed() {
        assert_eq!(could_be_cheaper_key(0), "could_be_cheaper_0");
        assert_eq!(could_be_cheaper_key(1), "could_be_cheaper_1");
    }

    #[test]
    fn could_be_cheaper_question_is_the_tested_wording() {
        let Question::Noul {
            instructions,
            criteria,
        } = could_be_cheaper_question("#1129")
        else {
            panic!("expected a noul question");
        };
        assert_eq!(
            instructions,
            "This issue cites #1129, which is still open. If #1129 is resolved, how likely is \
             it that LESS design work would remain for THIS issue than the current text \
             implies — as opposed to this issue's own remaining work being unaffected, because \
             it is already scoped separately, is a parallel/sibling effort, or #1129 is \
             otherwise not a precondition for finishing this issue's own remaining work?"
        );
        assert!(criteria.is_none());
    }

    #[test]
    fn dependency_entries_reads_the_noul_answer_per_citation() {
        let citations = vec![
            Citation {
                item_ref: ItemRef {
                    provider: GitProvider::GitHub,
                    project: "rust-works/omni-dev".to_string(),
                    kind: ItemKind::Issue,
                    number: 1129,
                },
                raw: "#1129".to_string(),
            },
            Citation {
                item_ref: ItemRef {
                    provider: GitProvider::GitHub,
                    project: "rust-works/omni-dev".to_string(),
                    kind: ItemKind::Issue,
                    number: 1349,
                },
                raw: "#1349".to_string(),
            },
        ];
        let answers = BTreeMap::from([(
            "could_be_cheaper_0".to_string(),
            Answer::Noul { noul: 0.75 },
        )]);
        let deps = dependency_entries("o/r#1", &citations, &answers);
        assert_eq!(deps.len(), 1, "{deps:?}");
        assert_eq!(deps[0].item_ref, "#1129");
        assert_eq!(deps[0].state, ItemState::Open);
        assert_eq!(deps[0].could_be_cheaper.get("design"), Some(&0.75));
    }

    #[test]
    fn stage_answers_reject_none_outside_design() {
        let choice = |c: &str| Answer::Choice {
            choice: c.to_string(),
            confidence: 0.5,
            probabilities: BTreeMap::new(),
        };
        let answers = BTreeMap::from([
            ("anthropic.stage_design".to_string(), choice("none")),
            ("anthropic.stage_implement".to_string(), choice("none")),
            ("anthropic.stage_review".to_string(), choice("opus")),
        ]);
        let err = stage_answers(&answers, &anthropic()[0]).unwrap_err();
        assert!(err.to_string().contains("not offered"), "{err}");
    }

    #[test]
    fn stage_answers_require_a_choice_per_stage() {
        let answers = BTreeMap::from([(
            "anthropic.stage_design".to_string(),
            Answer::Noul { noul: 0.5 },
        )]);
        let err = stage_answers(&answers, &anthropic()[0]).unwrap_err();
        assert!(err.to_string().contains("no choice answer"), "{err}");
        assert!(err.to_string().contains("anthropic.stage_design"), "{err}");
    }

    /// A ladder's answers are read under its own prefix, so one provider's
    /// answers can never satisfy another's questions.
    #[test]
    fn stage_answers_are_read_under_the_ladders_prefix() {
        let choice = |c: &str| Answer::Choice {
            choice: c.to_string(),
            confidence: 0.5,
            probabilities: BTreeMap::new(),
        };
        let answers = BTreeMap::from([
            ("anthropic.stage_design".to_string(), choice("none")),
            ("anthropic.stage_implement".to_string(), choice("sonnet")),
            ("anthropic.stage_review".to_string(), choice("opus")),
        ]);
        let err = stage_answers(&answers, &Ladder::builtin(Provider::OpenAi).unwrap()).unwrap_err();
        assert!(err.to_string().contains("openai.stage_design"), "{err}");
    }

    // ── validate_options ─────────────────────────────────────────────

    #[test]
    fn closed_issues_are_refused_by_default() {
        let err =
            validate_options(&[doc(1, ItemState::Closed)], &anthropic(), &opts()).unwrap_err();
        assert!(err.to_string().contains("rust-works/omni-dev#1"), "{err}");
        assert!(err.to_string().contains("--allow-closed"), "{err}");
    }

    #[test]
    fn closed_issues_are_allowed_on_request() {
        let mut o = opts();
        o.allow_closed = true;
        validate_options(&[doc(1, ItemState::Closed)], &anthropic(), &o).unwrap();
    }

    #[test]
    fn empty_runs_and_bad_knobs_are_rejected() {
        assert!(validate_options(&[], &anthropic(), &opts()).is_err());
        let mut o = opts();
        o.close_call = 1.5;
        assert!(validate_options(&[doc(1, ItemState::Open)], &anthropic(), &o).is_err());
        let mut o = opts();
        o.max_input_chars = 0;
        assert!(validate_options(&[doc(1, ItemState::Open)], &anthropic(), &o).is_err());
    }

    #[test]
    fn empty_and_duplicate_providers_are_rejected() {
        let err = validate_options(&[doc(1, ItemState::Open)], &[], &opts()).unwrap_err();
        assert!(err.to_string().contains("no providers"), "{err}");
        let twice = [
            Ladder::builtin(Provider::Gemini).unwrap(),
            Ladder::builtin(Provider::Gemini).unwrap(),
        ];
        let err = validate_options(&[doc(1, ItemState::Open)], &twice, &opts()).unwrap_err();
        assert!(
            err.to_string()
                .contains("\"gemini\" is listed more than once"),
            "{err}"
        );
    }

    // ── run_route (wiremock) ─────────────────────────────────────────

    fn choice_json(choice: &str, confidence: f64) -> serde_json::Value {
        serde_json::json!({
            "type": "choice", "choice": choice, "confidence": confidence,
            "probabilities": {choice: confidence}
        })
    }

    fn provider<'a>(outcome: &'a RouteOutcome, name: &str) -> &'a ProviderRoute {
        let RouteOutcome::Routed { providers, .. } = outcome else {
            panic!("expected a routed issue: {outcome:?}");
        };
        &providers[name]
    }

    #[tokio::test]
    async fn run_route_asks_three_stages_per_issue_and_sums_usage() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "model": "jev-latest",
                "state": "# #7 Route issues\n\nThe body.\n",
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "anthropic.stage_design": choice_json("fable", 0.52),
                        "anthropic.stage_implement": choice_json("sonnet", 0.83),
                        "anthropic.stage_review": choice_json("opus", 0.21),
                    },
                    "usage": {"input_tokens": 100, "output_tokens": 10}
                })),
            )
            .expect(2)
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();

        let report = run_route(
            &client,
            &[doc(7, ItemState::Open), doc(7, ItemState::Open)],
            &anthropic(),
            &opts(),
            &OpenDependencies::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.model, "jev-1.13.0");
        assert_eq!(report.usage.input_tokens, 200);
        assert_eq!(report.usage.output_tokens, 20);
        let issue = &report.issues[0];
        assert_eq!(issue.item_ref, "rust-works/omni-dev#7");
        let route = provider(&issue.outcome, "anthropic");
        assert_eq!(route.class, "fable");
        assert_eq!(route.close_calls, [Stage::Review]);
        assert!(!issue.truncated);

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[0].body_json().unwrap();
        let keys: Vec<&String> = body["questions"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "anthropic.stage_design",
                "anthropic.stage_implement",
                "anthropic.stage_review"
            ]
        );
    }

    /// Two ladders ride one request and come back as two independent
    /// routings; `depends_on` stays issue-level, asked once.
    #[tokio::test]
    async fn run_route_routes_every_provider_in_one_call() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "anthropic.stage_design": choice_json("fable", 0.52),
                        "anthropic.stage_implement": choice_json("sonnet", 0.83),
                        "anthropic.stage_review": choice_json("opus", 0.9),
                        "openai.stage_design": choice_json("none", 0.9),
                        "openai.stage_implement": choice_json("sol", 0.2),
                        "openai.stage_review": choice_json("astra", 0.9),
                        "could_be_cheaper_0": {"type": "noul", "noul": 0.4},
                    },
                    "usage": {"input_tokens": 10, "output_tokens": 1}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();
        let ladders = [
            Ladder::builtin(Provider::Anthropic).unwrap(),
            Ladder::builtin(Provider::OpenAi).unwrap(),
        ];
        let dependencies = OpenDependencies::from([(
            ("rust-works/omni-dev".to_string(), 7),
            vec![citation("#1129", 1129)],
        )]);

        let report = run_route(
            &client,
            &[doc(7, ItemState::Open)],
            &ladders,
            &opts(),
            &dependencies,
        )
        .await
        .unwrap();

        let outcome = &report.issues[0].outcome;
        let RouteOutcome::Routed {
            providers,
            depends_on,
        } = outcome
        else {
            panic!("expected a routed issue: {outcome:?}");
        };
        assert_eq!(
            providers.keys().collect::<Vec<_>>(),
            ["anthropic", "openai"]
        );
        assert_eq!(provider(outcome, "anthropic").class, "fable");
        assert!(provider(outcome, "anthropic").close_calls.is_empty());
        assert_eq!(provider(outcome, "openai").class, "sol");
        assert_eq!(provider(outcome, "openai").close_calls, [Stage::Implement]);
        assert_eq!(depends_on.len(), 1);

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[0].body_json().unwrap();
        let keys: Vec<&String> = body["questions"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "anthropic.stage_design",
                "anthropic.stage_implement",
                "anthropic.stage_review",
                "could_be_cheaper_0",
                "openai.stage_design",
                "openai.stage_implement",
                "openai.stage_review",
            ]
        );
    }

    /// A ladder whose answers are missing fails the issue, so a partly
    /// routed issue is never reported as routed.
    #[tokio::test]
    async fn run_route_fails_the_issue_when_one_provider_is_unanswered() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(routed_json()))
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();
        let ladders = [
            Ladder::builtin(Provider::Anthropic).unwrap(),
            Ladder::builtin(Provider::Gemini).unwrap(),
        ];
        let report = run_route(
            &client,
            &[doc(7, ItemState::Open)],
            &ladders,
            &opts(),
            &OpenDependencies::new(),
        )
        .await
        .unwrap();
        let RouteOutcome::Failed { error } = &report.issues[0].outcome else {
            panic!("expected a failed issue: {:?}", report.issues[0]);
        };
        assert!(error.contains("gemini.stage_design"), "{error}");
    }

    fn routed_json() -> serde_json::Value {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "anthropic.stage_design": choice_json("none", 0.9),
                "anthropic.stage_implement": choice_json("sonnet", 0.9),
                "anthropic.stage_review": choice_json("opus", 0.9),
            },
            "usage": {"input_tokens": 5, "output_tokens": 1}
        })
    }

    /// A failure on one issue is recorded on it; the others are still routed.
    #[tokio::test]
    async fn run_route_keeps_going_past_a_failed_issue() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"state": "# #1 Route issues\n\nThe body.\n"}),
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(routed_json()))
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();
        let report = run_route(
            &client,
            &[doc(1, ItemState::Open), doc(2, ItemState::Open)],
            &anthropic(),
            &opts(),
            &OpenDependencies::new(),
        )
        .await
        .unwrap();

        assert!(report.issues[0].failed());
        let RouteOutcome::Failed { error } = &report.issues[0].outcome else {
            panic!();
        };
        assert!(error.contains("500"), "{error}");
        assert!(!report.issues[1].failed());
        assert_eq!(report.usage.input_tokens, 5);
    }

    #[tokio::test]
    async fn run_route_stops_on_an_auth_failure() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("bad key"))
            .expect(1)
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();
        let err = run_route(
            &client,
            &[doc(3, ItemState::Open), doc(4, ItemState::Open)],
            &anthropic(),
            &opts(),
            &OpenDependencies::new(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("Failed to route issue rust-works/omni-dev#3"));
    }

    #[tokio::test]
    async fn run_route_names_the_issue_on_an_unexpected_answer() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "anthropic.stage_design": choice_json("haiku", 0.9),
                        "anthropic.stage_implement": choice_json("sonnet", 0.9),
                        "anthropic.stage_review": choice_json("opus", 0.9),
                    },
                })),
            )
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();
        let report = run_route(
            &client,
            &[doc(9, ItemState::Open)],
            &anthropic(),
            &opts(),
            &OpenDependencies::new(),
        )
        .await
        .unwrap();
        let RouteOutcome::Failed { error } = &report.issues[0].outcome else {
            panic!("expected a failed issue");
        };
        assert!(error.contains("Unexpected Jev answer"), "{error}");
        assert!(error.contains("not offered"), "{error}");
    }

    fn citation(raw: &str, number: u64) -> Citation {
        Citation {
            item_ref: ItemRef {
                provider: GitProvider::GitHub,
                project: "rust-works/omni-dev".to_string(),
                kind: ItemKind::Issue,
                number,
            },
            raw: raw.to_string(),
        }
    }

    #[tokio::test]
    async fn run_route_asks_could_be_cheaper_for_each_open_citation() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "anthropic.stage_design": choice_json("opus", 0.2),
                        "anthropic.stage_implement": choice_json("sonnet", 0.61),
                        "anthropic.stage_review": choice_json("opus", 0.9),
                        "could_be_cheaper_0": {"type": "noul", "noul": 0.75},
                    },
                    "usage": {"input_tokens": 10, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();
        let dependencies = OpenDependencies::from([(
            ("rust-works/omni-dev".to_string(), 7),
            vec![citation("#1129", 1129)],
        )]);

        let report = run_route(
            &client,
            &[doc(7, ItemState::Open)],
            &anthropic(),
            &opts(),
            &dependencies,
        )
        .await
        .unwrap();

        let RouteOutcome::Routed { depends_on, .. } = &report.issues[0].outcome else {
            panic!("expected a routed issue: {:?}", report.issues[0]);
        };
        assert_eq!(depends_on.len(), 1);
        assert_eq!(depends_on[0].item_ref, "#1129");
        assert_eq!(depends_on[0].state, ItemState::Open);
        assert_eq!(depends_on[0].could_be_cheaper.get("design"), Some(&0.75));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[0].body_json().unwrap();
        let keys: Vec<&String> = body["questions"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "anthropic.stage_design",
                "anthropic.stage_implement",
                "anthropic.stage_review",
                "could_be_cheaper_0",
            ]
        );
    }

    #[tokio::test]
    async fn run_route_drops_a_dependency_with_no_could_be_cheaper_answer() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(routed_json()))
            .mount(&server)
            .await;
        let client = JevClient::new(&server.uri(), "key").unwrap();
        let dependencies = OpenDependencies::from([(
            ("rust-works/omni-dev".to_string(), 7),
            vec![citation("#1129", 1129)],
        )]);

        let report = run_route(
            &client,
            &[doc(7, ItemState::Open)],
            &anthropic(),
            &opts(),
            &dependencies,
        )
        .await
        .unwrap();

        // routed_json() has no could_be_cheaper_0 answer; the issue still
        // routes, just with no dependencies reported.
        let RouteOutcome::Routed { depends_on, .. } = &report.issues[0].outcome else {
            panic!("expected a routed issue: {:?}", report.issues[0]);
        };
        assert!(depends_on.is_empty(), "{depends_on:?}");
    }

    #[test]
    fn report_serialises_ref_and_omits_false_truncated() {
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "o/r#1".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([(
                        "anthropic".to_string(),
                        ProviderRoute {
                            stages: stages("none", "sonnet", "opus"),
                            class: "sonnet".to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["issues"][0]["ref"], "o/r#1");
        assert!(value["issues"][0].get("truncated").is_none());
        let anthropic = &value["issues"][0]["providers"]["anthropic"];
        assert_eq!(anthropic["stages"]["design"]["choice"], "none");
        assert_eq!(anthropic["class"], "sonnet");
        assert_eq!(anthropic["close_calls"], serde_json::json!([]));
        assert!(value["issues"][0].get("stages").is_none());
        assert!(value["issues"][0].get("class").is_none());
        assert_eq!(value["issues"][0]["depends_on"], serde_json::json!([]));
        assert!(value["issues"][0].get("error").is_none());
    }

    #[test]
    fn a_failed_issue_serialises_only_its_error() {
        let route = IssueRoute {
            item_ref: "o/r#1".to_string(),
            url: "u".to_string(),
            title: "t".to_string(),
            outcome: RouteOutcome::Failed {
                error: "HTTP 529".to_string(),
            },
            truncated: false,
        };
        let value = serde_json::to_value(&route).unwrap();
        assert_eq!(value["error"], "HTTP 529");
        assert!(value.get("providers").is_none());
        assert!(value.get("depends_on").is_none());
    }
}
