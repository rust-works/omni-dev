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

pub mod effort;

/// The three stage questions, each missing its per-tier criteria.
const STAGE_QUESTIONS_YAML: &str = include_str!("../templates/jev-route-questions.yaml");

/// The design-stage option meaning "no design work remains". Reserved: no
/// tier may use this name.
pub const NO_DESIGN: &str = "none";

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

/// Citations whose individual GitHub lookup did not resolve.
///
/// Keyed by the citing issue's `(project, number)`, like
/// [`OpenDependencies`]. These are retained in route output so a typo or
/// stale reference is visible rather than being mistaken for no citation.
pub type ReferenceFetchFailures = BTreeMap<(String, u64), Vec<ReferenceFetchFailure>>;

/// A citation GitHub could not fetch while preparing a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReferenceFetchFailure {
    /// The citation exactly as written in the issue text.
    #[serde(rename = "ref")]
    pub item_ref: String,
    /// Why the reference could not be fetched.
    pub error: String,
}

/// One stage of the work on an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
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
    /// Concrete downstream models and their effort capabilities; absent for legacy ladders.
    pub models: Option<Vec<effort::Model>>,
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
        let mut model_rungs = BTreeMap::new();
        for tier in &file.tiers {
            if let Some(models) = &tier.models {
                effort::validate(models).with_context(|| format!("rung {:?}", tier.name))?;
                for model in models {
                    for &stage in &model.stages {
                        if let Some(previous) = model_rungs.insert((&model.name, stage), &tier.name)
                        {
                            bail!(
                                "model {:?} is bound to both rungs {previous:?} and {:?} for {stage:?}",
                                model.name,
                                tier.name
                            );
                        }
                    }
                }
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

/// One model ladder (#1820, #1826).
///
/// The name its answers are reported under and the tiers Jev chooses
/// between. A ladder is either a built-in provider's embedded ladder or a
/// custom one registered under its own name via `--ladder-definition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    /// The ladder's name: the output key and the question-key prefix.
    pub name: String,
    /// The tiers, least capable first.
    pub tiers: Tiers,
}

impl Ladder {
    /// A built-in provider's embedded ladder, named after the provider.
    pub fn builtin(provider: Provider) -> Result<Self> {
        Ok(Self {
            name: provider.name().to_string(),
            tiers: provider.tiers()?,
        })
    }

    /// A custom ladder registered under `name` (#1826).
    #[must_use]
    pub fn named(name: String, tiers: Tiers) -> Self {
        Self { name, tiers }
    }
}

/// Builds class and per-model effort questions for every ladder.
///
/// Class questions use the embedded wording with one criterion per tier,
/// keyed `<provider>.stage_<stage>`. All questions share one request.
pub fn build_route_questions(ladders: &[Ladder]) -> Result<BTreeMap<String, Question>> {
    let templates: BTreeMap<String, Question> = serde_yaml::from_str(STAGE_QUESTIONS_YAML)
        .context("Failed to parse the embedded stage questions")?;
    for stage in Stage::ALL {
        if !templates.contains_key(stage.template_key()) {
            bail!(
                "the embedded stage questions have no {:?}",
                // omni-dev: coverage ignore-line reason="defensive: the embedded questions YAML always has all three stage keys, pinned by stage_questions_are_the_tested_wording"
                stage.template_key()
            );
        }
    }
    let mut questions = BTreeMap::new();
    for ladder in ladders {
        for (key, template) in &templates {
            let mut question = template.clone();
            let Question::Choice { criteria, .. } = &mut question else {
                // omni-dev: coverage ignore-line reason="defensive: every embedded stage question is a choice question, pinned by stage_questions_are_the_tested_wording"
                bail!("embedded stage question {key:?} is not a choice question");
            };
            for tier in ladder.tiers.as_slice() {
                if criteria
                    .insert(tier.name.clone(), tier.description.clone())
                    .is_some()
                {
                    // omni-dev: coverage ignore-line reason="defensive: no embedded or custom tier is ever named `none`, the one fixed criterion (stage_design's no-design-work option)"
                    bail!(
                        "tier {:?} of ladder {:?} collides with a fixed option of {key:?}",
                        tier.name,
                        ladder.name
                    );
                }
            }
            question
                .validate()
                .with_context(|| format!("stage question {key:?} for {:?}", ladder.name))?;
            questions.insert(format!("{}.{key}", ladder.name), question);
        }
    }
    for ladder in ladders {
        effort::add_questions(ladder, &mut questions);
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
    /// Effort advice for every rung and its concrete models, independent of the selected rung.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub effort_by_model: BTreeMap<String, Vec<effort::ModelEffort>>,
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
    /// Resolved GitHub URL for terminal links; not part of the data format.
    #[serde(skip)]
    pub url: Option<String>,
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

/// What routing one issue produced.
///
/// Serialised untagged and flattened into [`IssueRoute`], so a routed issue
/// carries `providers`/`depends_on` and a failed one carries `error` and any
/// citation fetch failures.
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
        /// Citations that could not be fetched. They are not dependencies
        /// because their state is unknown, but remain visible to the user.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        reference_fetch_failures: Vec<ReferenceFetchFailure>,
    },
    /// The Jev call for this issue failed, or its answer was unusable. The
    /// other issues are still routed, so a long run keeps what it paid for.
    Failed {
        /// The error chain, on one line.
        error: String,
        /// Citations that could not be fetched before the Jev call failed.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        reference_fetch_failures: Vec<ReferenceFetchFailure>,
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
    run_route_with_reference_fetch_failures(
        jev,
        docs,
        ladders,
        opts,
        dependencies,
        &ReferenceFetchFailures::new(),
    )
    .await
}

/// Like [`run_route`], while retaining per-citation fetch failures in the
/// report. The CLI supplies this data after its blocking GitHub fetch.
pub async fn run_route_with_reference_fetch_failures(
    jev: &JevClient,
    docs: &[IssueDoc],
    ladders: &[Ladder],
    opts: &RouteOptions,
    dependencies: &OpenDependencies,
    reference_fetch_failures: &ReferenceFetchFailures,
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
        let citation_failures = reference_fetch_failures
            .get(&(doc.project.clone(), doc.number))
            .cloned()
            .unwrap_or_default();
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
            Err(err) => failed(
                &item_ref,
                &err.context("Jev request failed"),
                &citation_failures,
            ),
            Ok(response) => {
                models.insert(response.model);
                usage.input_tokens += response.usage.input_tokens;
                usage.output_tokens += response.usage.output_tokens;
                match provider_routes(&response.answers, ladders, opts.close_call) {
                    Ok(providers) => RouteOutcome::Routed {
                        providers,
                        depends_on: dependency_entries(&item_ref, citations, &response.answers),
                        reference_fetch_failures: citation_failures.clone(),
                    },
                    Err(err) => failed(
                        &item_ref,
                        &err.context("Unexpected Jev answer"),
                        &citation_failures,
                    ),
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
fn failed(
    item_ref: &str,
    err: &anyhow::Error,
    reference_fetch_failures: &[ReferenceFetchFailure],
) -> RouteOutcome {
    let error = format!("{err:#}");
    warn!("Could not route issue {item_ref}: {error}");
    RouteOutcome::Failed {
        error,
        reference_fetch_failures: reference_fetch_failures.to_vec(),
    }
}

/// Rejects an empty run, an empty or repeated ladder list, out-of-range
/// knobs, and (by default) closed issues, before any paid request is sent.
fn validate_options(docs: &[IssueDoc], ladders: &[Ladder], opts: &RouteOptions) -> Result<()> {
    if docs.is_empty() {
        bail!("no issues to route");
    }
    if ladders.is_empty() {
        bail!("no ladders to route against");
    }
    let mut seen = BTreeSet::new();
    for ladder in ladders {
        if !seen.insert(ladder.name.as_str()) {
            bail!("ladder {:?} is listed more than once", ladder.name);
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
            let mut stages = stage_answers(answers, ladder)?;
            for (stage, answer) in [
                (Stage::Design, &mut stages.design),
                (Stage::Implement, &mut stages.implement),
                (Stage::Review, &mut stages.review),
            ] {
                answer.effort_by_model =
                    effort::decode(ladder, stage, answer, answers, close_call)?;
            }
            let route = ProviderRoute {
                class: issue_class(&stages, &ladder.tiers),
                close_calls: close_calls(&stages, close_call),
                stages,
            };
            Ok((ladder.name.clone(), route))
        })
        .collect()
}

/// Reads one ladder's three stage answers out of a response, checking each
/// is a `choice` naming an option that was offered.
fn stage_answers(answers: &BTreeMap<String, Answer>, ladder: &Ladder) -> Result<StageAnswers> {
    let tiers = &ladder.tiers;
    let read = |stage: Stage| -> Result<StageAnswer> {
        let key = stage.question_key(&ladder.name);
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
            effort_by_model: BTreeMap::new(),
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
                    url: citation.url.clone(),
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

/// Renders `report` as one block per issue, blank-line separated, in request
/// order, followed by a trailing model/usage line (#1824).
///
/// Plain text, deliberately with no markdown: it's read directly in a
/// terminal, which doesn't render `**bold**`/`*italic*` markers, so they'd
/// just be clutter. Each issue is a header line (`ref — title`) followed by
/// one indented line per fact — one per provider's routing, then `depends_on`
/// — rather than one run-on sentence, which reads poorly once more than one
/// provider is involved.
///
/// `max_input_chars` is threaded in explicitly rather than read off
/// `RouteReport` (which doesn't carry it) so a truncated issue's block can
/// name the actual configured cap. This is still a pure, side-effect-free
/// function of its arguments, unit-tested directly with fixture reports.
///
/// The wording is **not** a stable contract: it may change without notice.
/// Scripts should use `json` or `yaml` instead.
#[must_use]
pub fn render_route_text(report: &RouteReport, max_input_chars: usize) -> String {
    render_route_text_styled(report, max_input_chars, &[], TerminalStyle::default())
}

/// Terminal features selected by the CLI. Passing them explicitly keeps the
/// renderer deterministic and lets callers retain plain output for pipes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalStyle {
    /// Emit SGR colour sequences.
    pub color: bool,
    /// Emit OSC 8 links around issue references.
    pub hyperlinks: bool,
}

impl TerminalStyle {
    /// Disables escape sequences for redirected output and dumb terminals.
    #[must_use]
    pub fn new(tty: bool, term: &str, no_color: bool, supports_links: bool) -> Self {
        let usable = tty && !term.is_empty() && term != "dumb";
        Self {
            color: usable && !no_color,
            hyperlinks: usable && supports_links,
        }
    }
}

/// Renders the human report with terminal presentation when supported.
#[must_use]
pub fn render_route_text_styled(
    report: &RouteReport,
    max_input_chars: usize,
    ladders: &[Ladder],
    style: TerminalStyle,
) -> String {
    let mut blocks: Vec<String> = report
        .issues
        .iter()
        .map(|issue| render_issue_block(issue, max_input_chars, ladders, style))
        .collect();
    blocks.push(format!(
        "model: {}, usage: {} input tokens, {} output tokens",
        report.model, report.usage.input_tokens, report.usage.output_tokens
    ));
    let mut text = blocks.join("\n\n");
    text.push('\n');
    text
}

/// Renders one issue's block: a header line, one indented line per provider's
/// routing (or its error), one `cites` line per open citation, and a
/// truncation note.
fn render_issue_block(
    issue: &IssueRoute,
    max_input_chars: usize,
    ladders: &[Ladder],
    style: TerminalStyle,
) -> String {
    let item_ref = hyperlink(&issue.item_ref, Some(&issue.url), style);
    let item_ref = if issue.failed() {
        item_ref
    } else {
        colorize(&item_ref, Complexity::Low, style)
    };
    let mut lines = vec![format!("{item_ref} — {}", issue.title)];
    match &issue.outcome {
        RouteOutcome::Routed {
            providers,
            depends_on,
            reference_fetch_failures,
        } => {
            for (provider, route) in providers {
                let ladder = ladders.iter().find(|ladder| ladder.name == *provider);
                lines.extend(render_provider_line(
                    provider,
                    route,
                    providers.len(),
                    ladder,
                    style,
                ));
            }
            lines.extend(render_depends_on_lines(depends_on, style));
            lines.extend(render_reference_fetch_failure_lines(
                reference_fetch_failures,
            ));
        }
        RouteOutcome::Failed {
            error,
            reference_fetch_failures,
        } => {
            lines.push(format!(
                "  {}",
                colorize(&format!("failed: {error}"), Complexity::High, style)
            ));
            lines.extend(render_reference_fetch_failure_lines(
                reference_fetch_failures,
            ));
        }
    }
    if issue.truncated {
        lines.push(format!(
            "  input truncated at {} characters",
            with_thousands(max_input_chars)
        ));
    }
    lines.join("\n")
}

/// Whether any of `route`'s three stage answers names a multi-model tier (a
/// comma-joined tier name from a custom ladder, #1826). The compact
/// single-line layout repeats a chosen tier name up to four times, which
/// becomes unreadable once that name is ~100 characters of comma-joined
/// model ids — so this is the trigger for the one-line-per-stage layout
/// (#1847). Checking the three stage choices is sufficient: `route.class` is
/// always one of `design`/`implement`'s choice (see `issue_class`), so it
/// can never be multi-model without one of those already being caught.
fn route_has_multi_model_tier(route: &ProviderRoute) -> bool {
    [
        &route.stages.design,
        &route.stages.implement,
        &route.stages.review,
    ]
    .into_iter()
    .any(|answer| answer.choice.contains(','))
}

/// Renders one provider's routing as one or more indented lines: the compact
/// single line (unchanged since #1824) when every chosen tier is
/// single-model, or one line per fact when a multi-model tier name would
/// otherwise repeat illegibly on one line (#1847).
fn render_provider_line(
    provider: &str,
    route: &ProviderRoute,
    provider_count: usize,
    ladder: Option<&Ladder>,
    style: TerminalStyle,
) -> Vec<String> {
    let mut lines = if route_has_multi_model_tier(route) {
        render_provider_block(provider, route, provider_count, ladder, style)
    } else {
        vec![render_provider_line_compact(
            provider,
            route,
            provider_count,
            ladder,
            style,
        )]
    };
    for stage in Stage::ALL {
        lines.extend(effort::render(
            stage,
            &route.stages.get(stage).effort_by_model,
            ladder,
        ));
    }
    lines
}

/// Renders one provider's routing as an indented line, e.g. `  sonnet —
/// design needs fable (0.52), ...` or, with more than one provider requested,
/// `  anthropic: sonnet — design needs fable (0.52), ...`.
fn render_provider_line_compact(
    provider: &str,
    route: &ProviderRoute,
    provider_count: usize,
    ladder: Option<&Ladder>,
    style: TerminalStyle,
) -> String {
    let class = color_choice(&route.class, ladder, style);
    let lead = if provider_count > 1 {
        format!("{provider}: {class}")
    } else {
        class
    };
    let clauses: Vec<String> = Stage::ALL
        .into_iter()
        .map(|stage| render_stage_clause(stage, &route.stages, &route.close_calls, ladder, style))
        .collect();
    format!("  {lead} — {}", clauses.join(", "))
}

/// Renders one stage's clause, e.g. `design needs fable (0.52)` or `review
/// opus (0.41, close call)`.
fn render_stage_clause(
    stage: Stage,
    stages: &StageAnswers,
    close_calls: &[Stage],
    ladder: Option<&Ladder>,
    style: TerminalStyle,
) -> String {
    let answer = stages.get(stage);
    let choice = color_choice(&answer.choice, ladder, style);
    let label = match stage {
        Stage::Design if answer.choice == NO_DESIGN => format!(
            "design needs {}",
            colorize("no further work", Complexity::Low, style)
        ),
        Stage::Design => format!("design needs {choice}"),
        Stage::Implement => format!("implementation {choice}"),
        Stage::Review => format!("review {choice}"),
    };
    format!(
        "{label} ({})",
        render_stage_confidence(answer, close_calls.contains(&stage))
    )
}

/// Adds the highest-probability alternative to a close call in either text
/// layout. Ties pick the alphabetically first option for stable output.
fn render_stage_confidence(answer: &StageAnswer, close_call: bool) -> String {
    let mut detail = format!("{:.2}", answer.confidence);
    if close_call {
        detail.push_str(", close call");
        if let Some((name, probability)) = answer
            .probabilities
            .iter()
            .filter(|(name, probability)| name.as_str() != answer.choice && probability.is_finite())
            .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        {
            detail.push_str(&format!(" — {name} {probability:.2}"));
        }
    }
    detail
}

/// Renders one provider's routing as one line per fact: `class:` once, then
/// `design:` / `implementation:` / `review:` — the one-line-per-stage layout
/// used when a chosen tier name is multi-model (#1847). With more than one
/// provider requested, the provider name heads the block instead of
/// prefixing every line, and the fact lines nest one indent level deeper.
fn render_provider_block(
    provider: &str,
    route: &ProviderRoute,
    provider_count: usize,
    ladder: Option<&Ladder>,
    style: TerminalStyle,
) -> Vec<String> {
    let mut lines = Vec::with_capacity(5);
    let indent = if provider_count > 1 {
        lines.push(format!("  {provider}:"));
        "    "
    } else {
        "  "
    };
    lines.push(format!(
        "{indent}class: {}",
        color_choice(&route.class, ladder, style)
    ));
    for stage in Stage::ALL {
        lines.push(format!(
            "{indent}{}",
            render_stage_line(stage, &route.stages, &route.close_calls, ladder, style)
        ));
    }
    lines
}

/// Renders one stage's fact line for [`render_provider_block`], e.g.
/// `design: needs no further work (0.94)` or `review: opus (0.41, close
/// call)` — parallel to [`render_stage_clause`] but with the stage name as a
/// `label:` prefix rather than folded into a clause.
fn render_stage_line(
    stage: Stage,
    stages: &StageAnswers,
    close_calls: &[Stage],
    ladder: Option<&Ladder>,
    style: TerminalStyle,
) -> String {
    let answer = stages.get(stage);
    let choice = color_choice(&answer.choice, ladder, style);
    let (label, content) = match stage {
        Stage::Design if answer.choice == NO_DESIGN => (
            "design",
            format!(
                "needs {}",
                colorize("no further work", Complexity::Low, style)
            ),
        ),
        Stage::Design => ("design", format!("needs {choice}")),
        Stage::Implement => ("implementation", choice),
        Stage::Review => ("review", choice),
    };
    format!(
        "{label}: {content} ({})",
        render_stage_confidence(answer, close_calls.contains(&stage))
    )
}

/// Renders one `  cites` line per open citation in `depends_on`, in citation
/// order. Empty when there are no open citations.
fn render_depends_on_lines(depends_on: &[DependencyEntry], style: TerminalStyle) -> Vec<String> {
    depends_on
        .iter()
        .map(|dep| {
            let item_ref = hyperlink(&dep.item_ref, dep.url.as_deref(), style);
            match dep.could_be_cheaper.get("design") {
                Some(prob) => format!(
                    "  cites open {item_ref}, which could leave less design work if resolved ({prob:.2})"
                ),
                None => format!("  cites open {item_ref}"),
            }
        })
        .collect()
}

/// Renders per-citation GitHub misses separately from open dependencies.
fn render_reference_fetch_failure_lines(failures: &[ReferenceFetchFailure]) -> Vec<String> {
    failures
        .iter()
        .map(|failure| {
            format!(
                "  reference fetch failed: {} ({})",
                failure.item_ref, failure.error
            )
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Complexity {
    Low,
    Medium,
    High,
}

/// Tier order, rather than provider-specific names, determines complexity.
fn choice_complexity(choice: &str, ladder: &Ladder) -> Option<Complexity> {
    if choice == NO_DESIGN {
        return Some(Complexity::Low);
    }
    let tiers = ladder.tiers.as_slice();
    let index = tiers.iter().position(|tier| tier.name == choice)?;
    Some(if index == 0 {
        Complexity::Low
    } else if index == tiers.len() - 1 {
        Complexity::High
    } else {
        Complexity::Medium
    })
}

fn color_choice(choice: &str, ladder: Option<&Ladder>, style: TerminalStyle) -> String {
    ladder
        .and_then(|ladder| choice_complexity(choice, ladder))
        .map_or_else(
            || choice.to_string(),
            |level| colorize(choice, level, style),
        )
}

fn colorize(text: &str, level: Complexity, style: TerminalStyle) -> String {
    if !style.color {
        return text.to_string();
    }
    let code = match level {
        Complexity::Low => 32,
        Complexity::Medium => 33,
        Complexity::High => 31,
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// Link only the visible reference, and only to a GitHub issue/PR URL. OSC 8
/// URLs must not contain characters that could terminate the control sequence.
fn hyperlink(text: &str, url: Option<&str>, style: TerminalStyle) -> String {
    let Some(url) = url.filter(|url| style.hyperlinks && valid_github_item_url(url)) else {
        return text.to_string();
    };
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

fn valid_github_item_url(raw: &str) -> bool {
    if raw.chars().any(char::is_control) {
        return false;
    }
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return false;
    }
    let Some(segments) = url.path_segments() else {
        return false; // omni-dev: coverage ignore-line reason="unreachable: https is a special scheme per the WHATWG URL spec, so a URL that already passed the scheme check above can never be cannot-be-a-base and path_segments() is always Some"
    };
    let parts: Vec<_> = segments.collect();
    parts.len() == 4
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && matches!(parts[2], "issues" | "pull")
        && parts[3].parse::<u64>().is_ok()
}

/// Groups `n`'s digits by thousands, e.g. `60_000` -> `"60,000"`.
fn with_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped.chars().rev().collect()
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

    // Existing class-only fixtures exercise backward compatibility; effort tests use real built-ins.
    fn class_only(provider: Provider) -> Result<Ladder> {
        let mut ladder = Ladder::builtin(provider)?;
        for tier in &mut ladder.tiers.0 {
            tier.models = None;
        }
        Ok(ladder)
    }

    fn anthropic() -> Vec<Ladder> {
        vec![class_only(Provider::Anthropic).unwrap()]
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
                    // omni-dev: coverage ignore-line reason="assert_eq!'s message args are only evaluated on failure, and this test always passes"
                    provider.name(),
                    rung.name,
                    reference.name
                );
            }
        }
    }

    #[test]
    fn ladders_are_named_by_provider_or_a_custom_name() {
        let ladder = class_only(Provider::Gemini).unwrap();
        assert_eq!(ladder.name, "gemini");
        let custom = Ladder::named("mine".to_string(), default_tiers());
        assert_eq!(custom.name, "mine");
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
            class_only(Provider::Anthropic).unwrap(),
            class_only(Provider::OpenAi).unwrap(),
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
    fn a_named_ladder_is_keyed_by_its_own_name() {
        let ladders = [Ladder::named(
            "mine".to_string(),
            Tiers::parse("tiers:\n  - {name: a, description: A}\n  - {name: b, description: B}\n")
                .unwrap(),
        )];
        let questions = build_route_questions(&ladders).unwrap();
        assert_eq!(options(&questions, "mine.stage_implement"), ["a", "b"]);
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
            effort_by_model: BTreeMap::new(),
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
    fn close_call_shows_the_best_alternative_in_compact_text() {
        let mut stages = stages("sol", "terra", "sol");
        stages.design.confidence = 0.21;
        stages.design.probabilities = BTreeMap::from([
            ("none".to_string(), 0.44),
            ("sol".to_string(), 0.48),
            ("terra".to_string(), 0.08),
        ]);

        assert_eq!(
            render_stage_clause(
                Stage::Design,
                &stages,
                &[Stage::Design],
                None,
                TerminalStyle::default(),
            ),
            "design needs sol (0.21, close call — none 0.44)"
        );
        assert_eq!(
            render_stage_clause(Stage::Design, &stages, &[], None, TerminalStyle::default(),),
            "design needs sol (0.21)"
        );
    }

    #[test]
    fn close_call_uses_a_stable_tiebreaker_and_handles_missing_alternatives() {
        let mut answer = answer("sol", 0.21);
        answer.probabilities = BTreeMap::from([
            ("astra".to_string(), 0.4),
            ("sol".to_string(), 0.2),
            ("terra".to_string(), 0.4),
        ]);
        assert_eq!(
            render_stage_confidence(&answer, true),
            "0.21, close call — astra 0.40"
        );
        answer.probabilities.clear();
        assert_eq!(render_stage_confidence(&answer, true), "0.21, close call");
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
                url: None,
            },
            Citation {
                item_ref: ItemRef {
                    provider: GitProvider::GitHub,
                    project: "rust-works/omni-dev".to_string(),
                    kind: ItemKind::Issue,
                    number: 1349,
                },
                raw: "#1349".to_string(),
                url: None,
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
        let err = stage_answers(&answers, &class_only(Provider::OpenAi).unwrap()).unwrap_err();
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
        assert!(err.to_string().contains("no ladders"), "{err}");
        let twice = [
            class_only(Provider::Gemini).unwrap(),
            class_only(Provider::Gemini).unwrap(),
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
            // omni-dev: coverage ignore-line reason="guards this test helper against misuse; every call site below passes an already-routed outcome"
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
            class_only(Provider::Anthropic).unwrap(),
            class_only(Provider::OpenAi).unwrap(),
        ];
        let dependencies = OpenDependencies::from([(
            ("rust-works/omni-dev".to_string(), 7),
            vec![citation("#1129", 1129)],
        )]);
        let reference_fetch_failures = ReferenceFetchFailures::from([(
            ("rust-works/omni-dev".to_string(), 7),
            vec![ReferenceFetchFailure {
                item_ref: "#404".to_string(),
                error: "not found".to_string(),
            }],
        )]);

        let report = run_route_with_reference_fetch_failures(
            &client,
            &[doc(7, ItemState::Open)],
            &ladders,
            &opts(),
            &dependencies,
            &reference_fetch_failures,
        )
        .await
        .unwrap();

        let outcome = &report.issues[0].outcome;
        let RouteOutcome::Routed {
            providers,
            depends_on,
            reference_fetch_failures,
            ..
        } = outcome
        else {
            // omni-dev: coverage ignore-line reason="guards this test's assumption; the mocked response above always answers with a routed outcome"
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
        assert_eq!(reference_fetch_failures[0].item_ref, "#404");

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
            class_only(Provider::Anthropic).unwrap(),
            class_only(Provider::Gemini).unwrap(),
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
        let RouteOutcome::Failed { error, .. } = &report.issues[0].outcome else {
            // omni-dev: coverage ignore-line reason="guards this test's assumption; the mocked response above always leaves gemini's answers missing"
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
        let reference_fetch_failures = ReferenceFetchFailures::from([(
            ("rust-works/omni-dev".to_string(), 1),
            vec![ReferenceFetchFailure {
                item_ref: "#404".to_string(),
                error: "not found".to_string(),
            }],
        )]);
        let report = run_route_with_reference_fetch_failures(
            &client,
            &[doc(1, ItemState::Open), doc(2, ItemState::Open)],
            &anthropic(),
            &opts(),
            &OpenDependencies::new(),
            &reference_fetch_failures,
        )
        .await
        .unwrap();

        assert!(report.issues[0].failed());
        let RouteOutcome::Failed {
            error,
            reference_fetch_failures,
        } = &report.issues[0].outcome
        else {
            panic!();
        };
        assert!(error.contains("500"), "{error}");
        assert_eq!(reference_fetch_failures[0].item_ref, "#404");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["issues"][0]["reference_fetch_failures"][0]["ref"],
            "#404"
        );
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(
            text.contains("  reference fetch failed: #404 (not found)"),
            "{text}"
        );
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
        let RouteOutcome::Failed { error, .. } = &report.issues[0].outcome else {
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
            url: None,
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
                    reference_fetch_failures: vec![],
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
        assert!(value["issues"][0].get("reference_fetch_failures").is_none());
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
                reference_fetch_failures: vec![],
            },
            truncated: false,
        };
        let value = serde_json::to_value(&route).unwrap();
        assert_eq!(value["error"], "HTTP 529");
        assert!(value.get("providers").is_none());
        assert!(value.get("depends_on").is_none());
    }

    // ── render_route_text (#1824) ──────────────────────────────────────

    #[test]
    fn render_route_text_matches_the_worked_example() {
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "rust-works/omni-dev#1641".to_string(),
                url: "u".to_string(),
                title: "Some issue title".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([(
                        "anthropic".to_string(),
                        ProviderRoute {
                            stages: StageAnswers {
                                design: answer("fable", 0.52),
                                implement: answer("sonnet", 0.83),
                                review: answer("opus", 0.41),
                            },
                            class: "fable".to_string(),
                            close_calls: vec![Stage::Review],
                        },
                    )]),
                    depends_on: vec![DependencyEntry {
                        item_ref: "#1129".to_string(),
                        url: None,
                        state: ItemState::Open,
                        could_be_cheaper: BTreeMap::from([("design".to_string(), 0.75)]),
                    }],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage {
                input_tokens: 1432,
                output_tokens: 61,
            },
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert_eq!(
            text,
            "rust-works/omni-dev#1641 — Some issue title\n\
             \x20\x20fable — design needs fable (0.52), implementation sonnet (0.83), review \
             opus (0.41, close call)\n\
             \x20\x20cites open #1129, which could leave less design work if resolved (0.75)\n\n\
             model: jev-1.13.0, usage: 1432 input tokens, 61 output tokens\n"
        );
    }

    #[test]
    fn render_route_text_puts_each_open_citation_on_its_own_line() {
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "rust-works/omni-dev#1845".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([(
                        "anthropic".to_string(),
                        ProviderRoute {
                            stages: stages("none", "sonnet", "sonnet"),
                            class: "sonnet".to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![
                        DependencyEntry {
                            item_ref: "#1830".to_string(),
                            url: None,
                            state: ItemState::Open,
                            could_be_cheaper: BTreeMap::from([("design".to_string(), 0.46)]),
                        },
                        DependencyEntry {
                            item_ref: "#1831".to_string(),
                            url: None,
                            state: ItemState::Open,
                            could_be_cheaper: BTreeMap::new(),
                        },
                    ],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(
            text.contains(
                "  cites open #1830, which could leave less design work if resolved (0.46)\n  \
                 cites open #1831\n"
            ),
            "{text}"
        );
        assert!(!text.contains("; "), "{text}");
    }

    #[test]
    fn render_route_text_names_each_provider_when_there_are_several() {
        let route = |class: &str| ProviderRoute {
            stages: stages("none", "sonnet", "sonnet"),
            class: class.to_string(),
            close_calls: vec![],
        };
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "o/r#1".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([
                        ("anthropic".to_string(), route("sonnet")),
                        ("openai".to_string(), route("terra")),
                    ]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(text.contains("  anthropic: sonnet —"), "{text}");
        assert!(text.contains("  openai: terra —"), "{text}");
    }

    #[test]
    fn render_route_text_reports_a_failed_issue() {
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "o/r#1".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Failed {
                    error: "HTTP 529".to_string(),
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(text.contains("o/r#1 — t\n  failed: HTTP 529"), "{text}");
    }

    #[test]
    fn render_route_text_omits_the_depends_on_sentence_when_empty() {
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
                            stages: stages("none", "sonnet", "sonnet"),
                            class: "sonnet".to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(!text.contains("cites"), "{text}");
    }

    /// A dependency with no `could_be_cheaper` probability for `design`
    /// (defensive: [`run_route`] always sets one for every entry it puts in
    /// `depends_on`) still renders, just without the parenthetical.
    #[test]
    fn render_route_text_cites_a_dependency_with_no_could_be_cheaper_probability() {
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
                            stages: stages("none", "sonnet", "sonnet"),
                            class: "sonnet".to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![DependencyEntry {
                        item_ref: "#1129".to_string(),
                        url: None,
                        state: ItemState::Open,
                        could_be_cheaper: BTreeMap::new(),
                    }],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(text.contains("cites open #1129"), "{text}");
        assert!(!text.contains("could leave less design work"), "{text}");
    }

    #[test]
    fn render_route_text_notes_a_truncated_issue_and_omits_the_note_otherwise() {
        let issue = |truncated: bool| IssueRoute {
            item_ref: "o/r#1".to_string(),
            url: "u".to_string(),
            title: "t".to_string(),
            outcome: RouteOutcome::Routed {
                providers: BTreeMap::from([(
                    "anthropic".to_string(),
                    ProviderRoute {
                        stages: stages("none", "sonnet", "sonnet"),
                        class: "sonnet".to_string(),
                        close_calls: vec![],
                    },
                )]),
                depends_on: vec![],
                reference_fetch_failures: vec![],
            },
            truncated,
        };
        let report = |truncated| RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![issue(truncated)],
            usage: Usage::default(),
        };
        let truncated_text = render_route_text(&report(true), 60_000);
        assert!(
            truncated_text.contains("  input truncated at 60,000 characters"),
            "{truncated_text}"
        );
        let kept_text = render_route_text(&report(false), 60_000);
        assert!(!kept_text.contains("truncated"), "{kept_text}");
    }

    #[test]
    fn render_route_text_notes_no_further_design_work() {
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
                            stages: stages("none", "sonnet", "sonnet"),
                            class: "sonnet".to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(text.contains("design needs no further work"), "{text}");
    }

    /// Pins the layout switch from #1847: a comma-joined (multi-model) tier
    /// name switches the block to one line per fact instead of repeating the
    /// name up to four times on one line.
    #[test]
    fn render_route_text_switches_to_one_line_per_stage_for_multi_model_tiers() {
        let multi = "global.anthropic.claude-sonnet-4-6,global.anthropic.claude-sonnet-5";
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "rust-works/omni-dev#1832".to_string(),
                url: "u".to_string(),
                title: "feat(drive): banded ranges for drive sheets (#1830)".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([(
                        "anthropic".to_string(),
                        ProviderRoute {
                            stages: StageAnswers {
                                design: answer(NO_DESIGN, 0.94),
                                implement: answer(multi, 0.94),
                                review: answer(multi, 0.70),
                            },
                            class: multi.to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![DependencyEntry {
                        item_ref: "#1830".to_string(),
                        url: None,
                        state: ItemState::Open,
                        could_be_cheaper: BTreeMap::from([("design".to_string(), 0.48)]),
                    }],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert_eq!(
            text,
            format!(
                "rust-works/omni-dev#1832 — feat(drive): banded ranges for drive sheets (#1830)\n\
                 \x20\x20class: {multi}\n\
                 \x20\x20design: needs no further work (0.94)\n\
                 \x20\x20implementation: {multi} (0.94)\n\
                 \x20\x20review: {multi} (0.70)\n\
                 \x20\x20cites open #1830, which could leave less design work if resolved (0.48)\n\n\
                 model: jev-1.13.0, usage: 0 input tokens, 0 output tokens\n"
            )
        );
    }

    /// A comma-free (single-model) choice keeps today's exact compact
    /// layout — no output churn for existing built-in-ladder users.
    #[test]
    fn render_route_text_keeps_the_compact_line_for_single_word_tiers() {
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
                            stages: stages("fable", "sonnet", "opus"),
                            class: "fable".to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(
            text.contains(
                "  fable — design needs fable (0.90), implementation sonnet (0.90), review \
                 opus (0.90)"
            ),
            "{text}"
        );
        assert!(!text.contains("class:"), "{text}");
    }

    /// With more than one ladder requested, a multi-model ladder's block is
    /// headed by its provider name rather than prefixing every line.
    #[test]
    fn render_route_text_headers_the_block_by_provider_when_multi_model_and_multiple_ladders() {
        let route = |class: &str| ProviderRoute {
            stages: StageAnswers {
                design: answer(NO_DESIGN, 0.9),
                implement: answer(class, 0.9),
                review: answer(class, 0.9),
            },
            class: class.to_string(),
            close_calls: vec![],
        };
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "o/r#1".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([
                        ("anthropic".to_string(), route("a,b")),
                        ("openai".to_string(), route("c,d")),
                    ]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(text.contains("  anthropic:\n    class: a,b\n"), "{text}");
        assert!(text.contains("    implementation: a,b (0.90)"), "{text}");
        assert!(text.contains("  openai:\n    class: c,d\n"), "{text}");
        assert!(text.contains("    implementation: c,d (0.90)"), "{text}");
    }

    /// The multi-line layout is decided per ladder, not for the whole issue:
    /// one provider with a multi-model choice does not force another
    /// provider's single-model choice into the block layout too.
    #[test]
    fn render_route_text_multi_model_layout_is_decided_per_ladder() {
        let multi_route = ProviderRoute {
            stages: StageAnswers {
                design: answer(NO_DESIGN, 0.9),
                implement: answer("a,b", 0.9),
                review: answer("a,b", 0.9),
            },
            class: "a,b".to_string(),
            close_calls: vec![],
        };
        let compact_route = ProviderRoute {
            stages: stages("none", "terra", "terra"),
            class: "terra".to_string(),
            close_calls: vec![],
        };
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "o/r#1".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([
                        ("anthropic".to_string(), multi_route),
                        ("openai".to_string(), compact_route),
                    ]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(text.contains("  anthropic:\n    class: a,b\n"), "{text}");
        assert!(
            text.contains("  openai: terra — design needs no further work"),
            "{text}"
        );
    }

    /// The close-call marker stays per stage in the multi-line layout too.
    #[test]
    fn render_route_text_notes_a_close_call_in_the_multi_line_layout() {
        let mut review = answer("a,b", 0.41);
        review.probabilities =
            BTreeMap::from([("a,b".to_string(), 0.56), ("c,d".to_string(), 0.44)]);
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
                            stages: StageAnswers {
                                design: answer(NO_DESIGN, 0.9),
                                implement: answer("a,b", 0.94),
                                review,
                            },
                            class: "a,b".to_string(),
                            close_calls: vec![Stage::Review],
                        },
                    )]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert!(
            text.contains("review: a,b (0.41, close call — c,d 0.44)"),
            "{text}"
        );
    }

    #[test]
    fn render_route_text_separates_multiple_issues_with_a_blank_line_in_request_order() {
        let issue = |item_ref: &str| IssueRoute {
            item_ref: item_ref.to_string(),
            url: "u".to_string(),
            title: "t".to_string(),
            outcome: RouteOutcome::Failed {
                error: "e".to_string(),
                reference_fetch_failures: vec![],
            },
            truncated: false,
        };
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![issue("o/r#1"), issue("o/r#2")],
            usage: Usage::default(),
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        let first = text.find("o/r#1").unwrap();
        let second = text.find("o/r#2").unwrap();
        assert!(first < second, "{text}");
        assert!(
            text.contains("o/r#1 — t\n  failed: e\n\no/r#2 — t\n  failed: e"),
            "{text}"
        );
    }

    #[test]
    fn render_route_text_ends_with_the_model_and_summed_usage() {
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
            },
        };
        let text = render_route_text(&report, DEFAULT_MAX_INPUT_CHARS);
        assert_eq!(
            text,
            "model: jev-1.13.0, usage: 10 input tokens, 20 output tokens\n"
        );
    }

    #[test]
    fn terminal_style_requires_a_usable_tty() {
        assert_eq!(
            TerminalStyle::new(false, "xterm", false, true),
            TerminalStyle::default()
        );
        assert_eq!(
            TerminalStyle::new(true, "dumb", false, true),
            TerminalStyle::default()
        );
        assert_eq!(
            TerminalStyle::new(true, "", false, true),
            TerminalStyle::default()
        );
        assert_eq!(
            TerminalStyle::new(true, "xterm", true, true),
            TerminalStyle {
                color: false,
                hyperlinks: true
            }
        );
        assert_eq!(
            TerminalStyle::new(true, "xterm", false, false),
            TerminalStyle {
                color: true,
                hyperlinks: false
            }
        );
    }

    #[test]
    fn complexity_uses_tier_order_for_built_in_and_custom_ladders() {
        let anthropic = class_only(Provider::Anthropic).unwrap();
        assert_eq!(
            choice_complexity(NO_DESIGN, &anthropic),
            Some(Complexity::Low)
        );
        assert_eq!(
            choice_complexity("sonnet", &anthropic),
            Some(Complexity::Low)
        );
        assert_eq!(
            choice_complexity("opus", &anthropic),
            Some(Complexity::Medium)
        );
        assert_eq!(
            choice_complexity("fable", &anthropic),
            Some(Complexity::High)
        );
        let custom = |names: &[&str]| {
            use std::fmt::Write;

            let mut yaml = String::from("tiers:\n");
            for name in names {
                writeln!(yaml, "  - {{name: {name}, description: work}}").unwrap();
            }
            Ladder::named("custom".to_string(), Tiers::parse(&yaml).unwrap())
        };
        let two = custom(&["a", "b"]);
        assert_eq!(choice_complexity("a", &two), Some(Complexity::Low));
        assert_eq!(choice_complexity("b", &two), Some(Complexity::High));
        let four = custom(&["a", "b", "c", "d"]);
        assert_eq!(choice_complexity("b", &four), Some(Complexity::Medium));
        assert_eq!(choice_complexity("c", &four), Some(Complexity::Medium));
        assert_eq!(choice_complexity("d", &four), Some(Complexity::High));
        assert_eq!(choice_complexity("unknown", &four), None);
    }

    #[test]
    fn hyperlink_uses_exact_osc_8_framing_and_rejects_unsafe_urls() {
        let style = TerminalStyle {
            color: false,
            hyperlinks: true,
        };
        let url = "https://github.com/other/repo/pull/42";
        assert_eq!(
            hyperlink("PR #42", Some(url), style),
            format!("\x1b]8;;{url}\x1b\\PR #42\x1b]8;;\x1b\\")
        );
        for bad in [
            "https://example.com/o/r/issues/1",
            "http://github.com/o/r/issues/1",
            "https://github.com/o/r/issues/1\x1b]8;;evil",
            "https://github.com/o/r/issues/nope",
            "not a valid url",
        ] {
            assert_eq!(hyperlink("#1", Some(bad), style), "#1");
        }
        assert_eq!(hyperlink("#1", None, style), "#1");
    }

    #[test]
    fn styled_report_colors_classes_stages_and_failure_and_links_refs() {
        let issue = IssueRoute {
            item_ref: "o/r#1".to_string(),
            url: "https://github.com/o/r/issues/1".to_string(),
            title: "t".to_string(),
            outcome: RouteOutcome::Routed {
                providers: BTreeMap::from([(
                    "anthropic".to_string(),
                    ProviderRoute {
                        stages: stages(NO_DESIGN, "opus", "fable"),
                        class: "opus".to_string(),
                        close_calls: vec![],
                    },
                )]),
                depends_on: vec![DependencyEntry {
                    item_ref: "#42".to_string(),
                    url: Some("https://github.com/other/repo/pull/42".to_string()),
                    state: ItemState::Open,
                    could_be_cheaper: BTreeMap::new(),
                }],
                reference_fetch_failures: vec![],
            },
            truncated: false,
        };
        let failed = IssueRoute {
            item_ref: "o/r#2".to_string(),
            url: "https://github.com/o/r/issues/2".to_string(),
            title: "failed".to_string(),
            outcome: RouteOutcome::Failed {
                error: "HTTP 529".to_string(),
                reference_fetch_failures: vec![],
            },
            truncated: false,
        };
        let report = RouteReport {
            model: "jev".to_string(),
            issues: vec![issue, failed],
            usage: Usage::default(),
        };
        let style = TerminalStyle {
            color: true,
            hyperlinks: true,
        };
        let text = render_route_text_styled(
            &report,
            DEFAULT_MAX_INPUT_CHARS,
            &[class_only(Provider::Anthropic).unwrap()],
            style,
        );
        assert!(
            text.contains(
                "\x1b[32m\x1b]8;;https://github.com/o/r/issues/1\x1b\\o/r#1\x1b]8;;\x1b\\\x1b[0m"
            ),
            "{text}"
        );
        assert!(
            text.contains("\x1b[33mopus\x1b[0m — design needs \x1b[32mno further work\x1b[0m"),
            "{text}"
        );
        assert!(text.contains("review \x1b[31mfable\x1b[0m"), "{text}");
        assert!(
            text.contains("\x1b]8;;https://github.com/other/repo/pull/42\x1b\\#42\x1b]8;;\x1b\\"),
            "{text}"
        );
        assert!(text.contains("\x1b[31mfailed: HTTP 529\x1b[0m"), "{text}");
        let plain = render_route_text_styled(
            &report,
            DEFAULT_MAX_INPUT_CHARS,
            &[],
            TerminalStyle::default(),
        );
        assert_eq!(plain, render_route_text(&report, DEFAULT_MAX_INPUT_CHARS));
        assert!(!plain.contains('\x1b'));
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("https://github.com/other/repo/pull/42"));
    }

    #[test]
    fn styled_multi_model_layout_colors_the_class_and_stage_choices() {
        let tier = "model-a,model-b";
        let ladder = Ladder::named(
            "custom".to_string(),
            Tiers::parse(&format!(
                "tiers:\n  - {{name: basic, description: work}}\n  - {{name: '{tier}', description: work}}\n"
            ))
            .unwrap(),
        );
        let report = RouteReport {
            model: "jev".to_string(),
            issues: vec![IssueRoute {
                item_ref: "o/r#1".to_string(),
                url: "https://github.com/o/r/issues/1".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([(
                        "custom".to_string(),
                        ProviderRoute {
                            stages: stages(NO_DESIGN, tier, "basic"),
                            class: tier.to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![],
                    reference_fetch_failures: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };
        let text = render_route_text_styled(
            &report,
            DEFAULT_MAX_INPUT_CHARS,
            &[ladder],
            TerminalStyle {
                color: true,
                hyperlinks: false,
            },
        );
        assert!(
            text.contains(&format!("  class: \x1b[31m{tier}\x1b[0m")),
            "{text}"
        );
        assert!(
            text.contains(&format!("  implementation: \x1b[31m{tier}\x1b[0m")),
            "{text}"
        );
        assert!(text.contains("  review: \x1b[32mbasic\x1b[0m"), "{text}");
    }

    #[test]
    fn design_stage_with_a_named_tier_renders_needs_prefix() {
        let line = render_stage_line(
            Stage::Design,
            &stages("fable", "sonnet", "sonnet"),
            &[],
            None,
            TerminalStyle::default(),
        );
        assert_eq!(line, "design: needs fable (0.90)");
    }

    #[test]
    fn with_thousands_groups_digits() {
        assert_eq!(with_thousands(0), "0");
        assert_eq!(with_thousands(999), "999");
        assert_eq!(with_thousands(60_000), "60,000");
        assert_eq!(with_thousands(1_000_000), "1,000,000");
    }
}
