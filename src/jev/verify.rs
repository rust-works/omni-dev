//! Decision-comment verification for `omni-dev ai jev verify-decision` (#1779).
//!
//! Splits a maintainer's decision comment into single, checkable statements
//! (via the configured AI backend), then checks each one against the
//! source it cites with one `noul` question per statement, one Jev call per
//! cited source. This is the wording and grouping validated in #1779's
//! per-statement experiment (E4): checking one statement at a time against
//! its source accepted 5/5 accurate claims and 0/20 wrong ones, where
//! checking a whole comment at once let overstatements through.
//!
//! A coverage check (new, unvalidated) additionally asks whether the split
//! statements together say everything the comment claims, guarding against
//! a splitter that drops or strengthens a claim.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use tracing::warn;

use crate::claude::client::ClaudeClient;
use crate::github_issues::{fetch_issues, fetch_items};
use crate::jev::citations::{find_citations, first_citation, Citation};
use crate::jev::client::JevClient;
use crate::jev::error::is_auth_failure;
use crate::jev::input::truncate_middle;
use crate::jev::protocol::{Answer, NoulCriteria, Question, SystemOneRequest, Usage};
use crate::provider::{Comment, IssueDoc, ItemKind, ItemRef};

/// Default threshold above which a statement counts as supported.
pub const DEFAULT_SUPPORTED: f64 = 0.5;
/// Default threshold below which a statement rejects the whole comment.
pub const DEFAULT_REJECT_BELOW: f64 = 0.3;
/// Default threshold below which low coverage is a review reason.
pub const DEFAULT_COVERAGE: f64 = 0.5;
/// Most statements one comment may be split into, before the splitter's
/// output is treated as broken rather than trusted.
pub const MAX_STATEMENTS: usize = 100;

// ── Citations ──────────────────────────────────────────────────────────

/// Resolves a split statement's raw `cites` text to the index, in `sources`,
/// of the source it names — by re-running the citation parser on that one
/// string and matching its `(project, number)` against every source's
/// [`Source::keys`], not by comparing display strings (which the splitter
/// might not reproduce with identical spacing or case).
fn resolve_cites_index(cites: &str, default_project: &str, sources: &[Source]) -> Option<usize> {
    let (project, _kind, number) = first_citation(cites, default_project)?;
    sources
        .iter()
        .position(|s| s.keys.contains(&(project.clone(), number)))
}

/// Whether a split statement's `cites` names the judged issue itself.
///
/// Rule 6 of the splitter prompt asks for `cites: null` on a statement
/// about the judged issue, and a live run showed the splitter sometimes
/// names it anyway. The judged issue is never a [`Source`] (a
/// self-citation is dropped by [`find_citations`]), so without this the
/// statement would be reported as an unresolved citation and drag the
/// verdict to `needs_review` — for obeying the rule in substance.
fn cites_judged_issue(cites: &str, judged: &IssueDoc) -> bool {
    first_citation(cites, &judged.project).is_some_and(|(project, _kind, number)| {
        project == judged.project && number == judged.number
    })
}

// ── Comment selection ──────────────────────────────────────────────────

/// Which decision comment `verify-decision` should check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentSelector {
    /// The most recent human comment that cites another issue or pull
    /// request.
    Latest,
    /// A specific comment, by its GitHub database id.
    Id(u64),
}

/// Parses `--comment`'s value: `latest`, a bare numeric id, or a full
/// issue-comment URL (`...#issuecomment-<id>`, as GitHub links a comment).
pub fn parse_comment_selector(raw: &str) -> Result<CommentSelector> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("latest") {
        return Ok(CommentSelector::Latest);
    }
    if let Ok(id) = raw.parse::<u64>() {
        return Ok(CommentSelector::Id(id));
    }
    if let Some(digits) = raw.strip_prefix_last("#issuecomment-") {
        if let Ok(id) = digits.parse::<u64>() {
            return Ok(CommentSelector::Id(id));
        }
    }
    bail!(
        "`{raw}` is not a recognised comment selector \
         (expected `latest`, a numeric comment id, or an issue-comment URL)"
    );
}

/// Splits on the *last* occurrence of `sep`, returning what follows it.
trait StripPrefixLast {
    fn strip_prefix_last<'a>(&'a self, sep: &str) -> Option<&'a str>;
}
impl StripPrefixLast for str {
    fn strip_prefix_last<'a>(&'a self, sep: &str) -> Option<&'a str> {
        self.rfind(sep).map(|i| &self[i + sep.len()..])
    }
}

/// Selects the comment `verify-decision` should check.
///
/// [`CommentSelector::Latest`] picks the most recent human comment that
/// cites another issue or pull request; a comment that cites nothing
/// verifiable would go straight to `needs_review` anyway, so those are
/// skipped rather than selected by default.
pub fn select_comment<'a>(
    doc: &'a IssueDoc,
    selector: &CommentSelector,
    default_project: &str,
) -> Result<&'a Comment> {
    let judged = ItemRef {
        provider: doc.provider,
        project: doc.project.clone(),
        kind: doc.kind,
        number: doc.number,
    };
    match selector {
        CommentSelector::Id(id) => {
            doc.comments
                .iter()
                .find(|c| c.id == Some(*id))
                .ok_or_else(|| {
                    anyhow!(
                        "issue {}#{} has no comment with id {id}",
                        doc.project,
                        doc.number
                    )
                })
        }
        CommentSelector::Latest => doc
            .comments
            .iter()
            .rev()
            .find(|c| !find_citations(&c.body, default_project, &judged).is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "issue {}#{} has no comment that cites another issue or pull request; \
                     pass --comment ID to select one explicitly",
                    doc.project,
                    doc.number
                )
            }),
    }
}

// ── Sources ────────────────────────────────────────────────────────────

/// One cited source: a cited issue plus the pull requests that closed it,
/// or a standalone cited pull request.
///
/// Matches the grouping validated in #1779's per-statement experiment
/// (E4), whose `SOURCES` names each key as exactly this pairing.
#[derive(Debug, Clone)]
pub struct Source {
    /// How a statement's `cites` string names this source, e.g. `"#1614"`.
    pub label: String,
    /// The full display name, e.g. `"#1614 and PR #1629"`.
    pub name: String,
    /// Display label for every item making up this source.
    pub items: Vec<String>,
    /// Every `(project, number)` that resolves to this source: the issue
    /// itself (or the standalone PR) plus any merged-in closing PRs.
    pub keys: Vec<(String, u64)>,
    /// The source text, in the tested `## SOURCE: ...` format, untruncated.
    pub text: String,
}

fn ref_label(project: &str, number: u64, judged_project: &str) -> String {
    if project == judged_project {
        format!("#{number}")
    } else {
        format!("{project}#{number}")
    }
}

fn pr_label(project: &str, number: u64, judged_project: &str) -> String {
    if project == judged_project {
        format!("PR #{number}")
    } else {
        format!("PR {project}#{number}")
    }
}

fn issue_heading(doc: &IssueDoc, judged_project: &str) -> String {
    if doc.project == judged_project {
        format!("# Issue #{}: {}", doc.number, doc.title)
    } else {
        format!("# Issue {}#{}: {}", doc.project, doc.number, doc.title)
    }
}

fn pr_heading(doc: &IssueDoc, judged_project: &str) -> String {
    if doc.project == judged_project {
        format!("# Pull request #{}: {}", doc.number, doc.title)
    } else {
        format!(
            "# Pull request {}#{}: {}",
            doc.project, doc.number, doc.title
        )
    }
}

/// Builds a cited issue's source text, in the format validated by E4's
/// `source_text`: the issue heading and body, then each human comment
/// (single leading newline — deliberately *not* `route`'s two-newline
/// separator, which was never part of this experiment), then each closing
/// pull request's own heading and body.
fn build_issue_source_text(
    doc: &IssueDoc,
    prs: &[IssueDoc],
    judged_project: &str,
    name: &str,
) -> String {
    let mut s = format!("{}\n\n{}\n", issue_heading(doc, judged_project), doc.body);
    for c in &doc.comments {
        s.push_str(&format!("\n**Comment by {}:**\n\n{}\n", c.author, c.body));
    }
    for pr in prs {
        s.push_str(&format!(
            "\n\n{}\n\n{}\n",
            pr_heading(pr, judged_project),
            pr.body
        ));
    }
    format!("## SOURCE: {name}\n\n{}\n", s.trim())
}

/// Builds a standalone cited pull request's source text. Unlike
/// [`build_issue_source_text`], this shape was never exercised by the
/// experiment — every source it tested was an issue (with or without
/// closing PRs) — so it is new, not tested.
fn build_pr_source_text(doc: &IssueDoc, judged_project: &str, name: &str) -> String {
    let s = format!("{}\n\n{}\n", pr_heading(doc, judged_project), doc.body);
    format!("## SOURCE: {name}\n\n{}\n", s.trim())
}

/// Groups a decision comment's citations into [`Source`]s.
///
/// One source per cited issue (merged with the pull requests that closed
/// it), plus one per standalone cited pull request not already merged
/// into an issue's source. `fetched` holds every successfully resolved
/// citation and closing PR, keyed by `(project, number)`; a citation
/// missing from it (not found, or its fetch failed) simply produces no
/// source, so a statement citing it surfaces later as unresolved rather
/// than failing the whole run.
#[must_use]
pub fn group_sources(
    citations: &[Citation],
    judged_project: &str,
    fetched: &BTreeMap<(String, u64), IssueDoc>,
) -> Vec<Source> {
    let mut sources = Vec::new();
    let mut consumed_prs: BTreeSet<(String, u64)> = BTreeSet::new();
    let mut seen_issues: BTreeSet<(String, u64)> = BTreeSet::new();

    for citation in citations {
        let key = (citation.item_ref.project.clone(), citation.item_ref.number);
        let Some(doc) = fetched.get(&key) else {
            continue;
        };
        if doc.kind != ItemKind::Issue || !seen_issues.insert(key.clone()) {
            continue;
        }
        let mut keys = vec![key.clone()];
        let mut items = vec![ref_label(&doc.project, doc.number, judged_project)];
        let mut pr_docs = Vec::new();
        for pr_ref in &doc.closed_by {
            let pr_key = (pr_ref.project.clone(), pr_ref.number);
            if let Some(pr_doc) = fetched.get(&pr_key) {
                items.push(pr_label(&pr_doc.project, pr_doc.number, judged_project));
                pr_docs.push(pr_doc.clone());
                keys.push(pr_key.clone());
                consumed_prs.insert(pr_key);
            }
        }
        let label = ref_label(&doc.project, doc.number, judged_project);
        let name = if pr_docs.is_empty() {
            label.clone()
        } else {
            format!("{label} and {}", items[1..].join(" and "))
        };
        let text = build_issue_source_text(doc, &pr_docs, judged_project, &name);
        sources.push(Source {
            label,
            name,
            items,
            keys,
            text,
        });
    }

    let mut seen_prs: BTreeSet<(String, u64)> = BTreeSet::new();
    for citation in citations {
        let key = (citation.item_ref.project.clone(), citation.item_ref.number);
        let Some(doc) = fetched.get(&key) else {
            continue;
        };
        if doc.kind != ItemKind::ChangeRequest
            || consumed_prs.contains(&key)
            || !seen_prs.insert(key.clone())
        {
            continue;
        }
        let label = pr_label(&doc.project, doc.number, judged_project);
        let text = build_pr_source_text(doc, judged_project, &label);
        sources.push(Source {
            label: label.clone(),
            name: label.clone(),
            items: vec![label],
            keys: vec![key],
            text,
        });
    }
    sources
}

// ── Fetching (blocking) ────────────────────────────────────────────────

/// Fetches everything `verify-decision` needs for one issue.
///
/// Fetches the judged issue, the selected comment, its citations, and the
/// [`Source`]s those citations resolve to (a first fetch of the cited
/// items, then a second for any closing pull requests not already
/// fetched). **Blocking** — callers must be on a blocking thread.
pub fn fetch_verify_input(
    bin: &Path,
    default_project: &str,
    judged: &ItemRef,
    selector: &CommentSelector,
) -> Result<(IssueDoc, Comment, Vec<Citation>, Vec<Source>)> {
    let issue = fetch_issues(bin, std::slice::from_ref(judged))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("issue {judged} missing from the parsed gh reply (bug)"))?;
    let comment = select_comment(&issue, selector, default_project)?.clone();
    let citations = find_citations(&comment.body, default_project, judged);
    if citations.is_empty() {
        return Ok((issue, comment, citations, Vec::new()));
    }

    let mut seen = HashSet::new();
    let refs: Vec<ItemRef> = citations
        .iter()
        .map(|c| c.item_ref.clone())
        .filter(|r| seen.insert((r.project.clone(), r.number)))
        .collect();
    let mut fetched: BTreeMap<(String, u64), IssueDoc> = BTreeMap::new();
    for (item_ref, doc) in refs.iter().zip(fetch_items(bin, &refs)?) {
        if let Some(doc) = doc {
            fetched.insert((item_ref.project.clone(), item_ref.number), doc);
        }
    }

    let mut seen_pr = HashSet::new();
    let pr_refs: Vec<ItemRef> = fetched
        .values()
        .filter(|doc| doc.kind == ItemKind::Issue)
        .flat_map(|doc| doc.closed_by.iter())
        .filter(|pr| !fetched.contains_key(&(pr.project.clone(), pr.number)))
        .filter(|pr| seen_pr.insert((pr.project.clone(), pr.number)))
        .cloned()
        .collect();
    if !pr_refs.is_empty() {
        for (item_ref, doc) in pr_refs.iter().zip(fetch_items(bin, &pr_refs)?) {
            if let Some(doc) = doc {
                fetched.insert((item_ref.project.clone(), item_ref.number), doc);
            }
        }
    }

    let sources = group_sources(&citations, &issue.project, &fetched);
    Ok((issue, comment, citations, sources))
}

// ── Statement splitting (AI backend) ───────────────────────────────────

/// One statement produced by the AI splitter, matching the reply shape
/// [`build_split_user_prompt`] asks for.
#[derive(Debug, Clone, serde::Deserialize)]
struct SplitStatement {
    text: String,
    cites: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SplitResponse {
    statements: Vec<SplitStatement>,
}

/// The splitter's system prompt (new, not validated against Jev — only the
/// per-statement and coverage *questions* were validated; the split itself
/// is new work for this command).
const SPLIT_SYSTEM_PROMPT: &str =
    "You split a maintainer's decision comment into single checkable \
     factual statements. Reply with the requested object and nothing else.";

/// Builds the splitter's user prompt.
///
/// Rules 1–7 are #1779's wording; the rest was added after a live run of
/// this command's own pipeline over the issue's 25 claim comments (see
/// `docs/jev.md`). Two additions are load-bearing, and both fixed a
/// measured failure rather than a hypothetical one:
///
/// - **Attribution of the judged issue.** "This issue was resolved by
///   #1655" is a claim about the judged issue, not about #1655, so
///   checking it against #1655 fails it. That rejected 3 of 10 accurate
///   comments before rule 6 gained its second paragraph. The judged
///   issue is named as "this issue" rather than by number on purpose:
///   giving the number made the splitter write it into statements the
///   comment never numbered, which the coverage check then read as added
///   content and marked down (9 faithful splits fell below the coverage
///   threshold).
/// - **The reply shape.** Without it the splitter answers in whatever
///   shape it likes whenever the schema is not enforced — every run
///   failed to parse on the schema-less path, which is what `ollama` and
///   the #1561 `output_config` fallback both take.
fn build_split_user_prompt(comment_body: &str, citations: &[Citation]) -> String {
    let cited = if citations.is_empty() {
        "(none)".to_string()
    } else {
        citations
            .iter()
            .map(|c| c.raw.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "Split the COMMENT below into single factual statements, so that each statement can be checked\n\
         on its own against the item it cites.\n\n\
         Rules:\n\
         1. One fact per statement. Split \"A and B\" into two statements.\n\
         2. Make every statement self-contained: name the item it is about (for example \"PR #1629 added\n\
         ...\"), replace pronouns with what they refer to, and do not depend on other statements.\n\
         3. Keep certainty exactly as written. \"was decided\", \"was considered\", \"is deferred\", \"is tracked\n\
         separately\" and \"may happen later\" are different facts. Never make a claim stronger or weaker.\n\
         4. Do not add, infer, summarise or correct anything. If the comment says something wrong, keep\n\
         the wrong claim as written.\n\
         5. Include every factual claim about a cited item, including conclusions such as \"so the open\n\
         question is settled\"; restate exactly what the comment says was settled.\n\
         6. Set \"cites\" to the one item the statement is about, written as it appears in the comment\n\
         (for example \"#1614\" or \"PR #1629\"). If the statement is a decision about the judged issue\n\
         itself rather than a claim about a cited item, set \"cites\" to null.\n\
         A statement that a cited item resolved, settled or answered the judged issue's question is\n\
         about the judged issue, so its \"cites\" is null. A statement about what a cited item did or\n\
         decided cites that item, even when it also mentions another item (such as a tracking issue).\n\
         7. Leave out greetings, opinions and instructions that make no factual claim.\n\n\
         The COMMENT was posted on the judged issue, which the comment calls \"this issue\". Refer to it\n\
         the same way; do not add its number.\n\n\
         COMMENT:\n{comment}\n\n\
         Items the comment cites: {cited}\n\n\
         Reply with an object whose \"statements\" field is a list; each entry has a \"text\" string and\n\
         a \"cites\" string or null. For example:\n\
         {{\"statements\": [{{\"text\": \"PR #1629 added a tool.\", \"cites\": \"PR #1629\"}}]}}",
        comment = comment_body.trim(),
    )
}

/// Strips a Markdown code fence (` ```json `, ` ```yaml ` or bare ` ``` `)
/// around the splitter's reply, for a backend that wraps it in prose
/// despite being asked for the object alone.
fn strip_code_fence(s: &str) -> &str {
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```yaml"))
        .or_else(|| s.strip_prefix("```"))
        .map_or(s, str::trim_start);
    s.strip_suffix("```").map_or(s, str::trim_end)
}

fn parse_split_response(raw: &str) -> Result<SplitResponse> {
    let trimmed = raw.trim();
    if let Ok(parsed) = serde_json::from_str(trimmed) {
        return Ok(parsed);
    }
    serde_yaml::from_str(strip_code_fence(trimmed))
        .with_context(|| format!("Failed to parse the statement splitter's response: {trimmed}"))
}

/// Splits `comment_body` into statements via the AI backend, validating the
/// result: non-empty, no more than [`MAX_STATEMENTS`], and no blank
/// statement text.
async fn split_statements(
    ai: &ClaudeClient,
    comment_body: &str,
    citations: &[Citation],
) -> Result<Vec<SplitStatement>> {
    let user_prompt = build_split_user_prompt(comment_body, citations);
    let raw = ai
        .send_message(SPLIT_SYSTEM_PROMPT, &user_prompt)
        .await
        .context("Failed to split the decision comment into statements")?;
    let parsed = parse_split_response(&raw)?;
    if parsed.statements.is_empty() {
        bail!("the statement splitter returned no statements");
    }
    if parsed.statements.len() > MAX_STATEMENTS {
        bail!(
            "the statement splitter returned {} statements, more than the {MAX_STATEMENTS} limit",
            parsed.statements.len()
        );
    }
    if parsed.statements.iter().any(|s| s.text.trim().is_empty()) {
        bail!("the statement splitter returned an empty statement");
    }
    Ok(parsed.statements)
}

// ── Jev questions ──────────────────────────────────────────────────────

/// Builds the per-statement `noul` question, reproducing #1779's tested
/// wording (E4's `STATEMENT_Q`/`STATEMENT_CRIT`) verbatim.
fn statement_question(text: &str) -> Question {
    Question::Noul {
        instructions: format!(
            "Is the following single STATEMENT supported by the SOURCE text? Answer true only if the source \
             states it or directly shows it. Answer false if the source contradicts it, if the source leaves \
             it open, optional, deferred or merely possible, or if the source does not mention it. STATEMENT: {text}"
        ),
        criteria: Some(NoulCriteria {
            true_means: Some("The source supports the statement".to_string()),
            false_means: Some("The source contradicts it, leaves it open, or does not mention it".to_string()),
        }),
    }
}

/// Builds the coverage `noul` question, reproducing #1779's tested wording
/// verbatim (new relative to E4: this check itself was not part of the
/// validated experiment, only its wording as specified in the issue).
fn coverage_question() -> Question {
    Question::Noul {
        instructions: "Do the numbered STATEMENTS together say everything the COMMENT claims, with nothing \
             added, strengthened or weakened? Answer false if any claim in the comment is missing, if any \
             statement adds something the comment does not say, or if any statement changes how certain or \
             final a claim is (for example \"considered\" becoming \"decided\")."
            .to_string(),
        criteria: Some(NoulCriteria {
            true_means: Some("The statements faithfully cover the comment".to_string()),
            false_means: Some("A claim is missing, added, or changed in certainty".to_string()),
        }),
    }
}

fn build_coverage_state(comment_body: &str, statements: &[SplitStatement]) -> String {
    let mut s = format!("## COMMENT\n\n{}\n\n## STATEMENTS\n\n", comment_body.trim());
    for (i, st) in statements.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, st.text));
    }
    s
}

// ── Report types ───────────────────────────────────────────────────────

/// The result of checking one decision comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every statement was checked, scored at least the supported
    /// threshold, and the coverage check ran and passed.
    Accepted,
    /// At least one statement scored below the reject threshold.
    Rejected,
    /// Nothing was definitely wrong, but something needs a person to look:
    /// an uncertain statement, an unresolved citation, low coverage, or a
    /// comment that cited nothing verifiable.
    NeedsReview,
}

/// Identifies the comment that was checked.
#[derive(Debug, Clone, Serialize)]
pub struct CommentRef {
    /// The comment's GitHub database id.
    pub id: Option<u64>,
    /// The comment's author.
    pub author: Option<String>,
}

/// One statement's verification outcome.
#[derive(Debug, Clone, Serialize)]
pub struct VerifiedStatement {
    /// The statement's text, as split by the AI backend.
    pub text: String,
    /// The source this statement was checked against, or `None` for a
    /// statement the splitter marked as a decision about the judged issue
    /// itself (`cites: null`) — reported, not verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The probability Jev assigned that the source supports this
    /// statement. `None` when the statement was never checked (no source,
    /// or the source's call failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported: Option<f64>,
    /// Why this statement affected the verdict, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One source that was (or was meant to be) checked against.
#[derive(Debug, Clone, Serialize)]
pub struct SourceReport {
    /// The source's display name, e.g. `"#1614 and PR #1629"`.
    pub source: String,
    /// Every item making up this source.
    pub items: Vec<String>,
    /// Whether this source's text was cut at the input cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Why this source could not be checked, if it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The models that answered.
///
/// Either field is omitted when that model never answered: a comment
/// citing nothing verifiable returns before any call is made, and naming
/// a model there would claim a request that never happened. `jev` is the
/// concrete version the API reports (`jev-1.13.0`), not the requested
/// alias.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportModels {
    /// The Jev model that answered.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub jev: String,
    /// The AI backend model that split the comment.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ai: String,
}

/// Token usage summed over every call.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportUsage {
    /// Jev usage, summed over every source's call and the coverage call.
    pub jev: Usage,
}

/// The result of verifying one decision comment.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    /// The judged issue, as `owner/repo#N`.
    #[serde(rename = "issue")]
    pub item_ref: String,
    /// The issue's web URL.
    pub url: String,
    /// The comment that was checked.
    pub comment: CommentRef,
    /// The overall verdict.
    pub verdict: Verdict,
    /// Why the verdict is what it is; empty for a clean `accepted`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// The coverage check's probability, or `None` if it could not be run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    /// Every source that was checked against.
    pub sources: Vec<SourceReport>,
    /// Every statement the comment was split into.
    pub statements: Vec<VerifiedStatement>,
    /// The models that answered.
    pub models: ReportModels,
    /// Token usage.
    pub usage: ReportUsage,
}

/// Knobs for [`run_verify`].
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    /// The Jev model to request.
    pub jev_model: String,
    /// Threshold above which a statement counts as supported.
    pub supported: f64,
    /// Threshold below which a statement rejects the comment outright.
    pub reject_below: f64,
    /// Threshold below which low coverage becomes a review reason.
    pub coverage: f64,
    /// Cap, in characters, on each source's text.
    pub max_input_chars: usize,
}

fn validate_verify_options(opts: &VerifyOptions) -> Result<()> {
    if !(0.0..=1.0).contains(&opts.reject_below) || !(0.0..=1.0).contains(&opts.supported) {
        bail!("--threshold and --reject-below must each be between 0 and 1");
    }
    if opts.reject_below > opts.supported {
        bail!("--reject-below must not be greater than --threshold");
    }
    if !(0.0..=1.0).contains(&opts.coverage) {
        bail!("--coverage-threshold must be between 0 and 1");
    }
    if opts.max_input_chars == 0 {
        bail!("the input cap must be at least 1 character");
    }
    Ok(())
}

/// Verifies one decision comment against the sources it cites.
///
/// A comment with no citations (or none that resolved to a fetched source)
/// is `needs_review` without any Jev or AI call — there is nothing to check.
/// Otherwise: the comment is split into statements (one AI call), each
/// statement is checked against its cited source (one Jev call per source,
/// covering every statement attributed to it), and a coverage check asks
/// whether the statements together say everything the comment claims (one
/// more Jev call). Any statement scoring below `opts.reject_below` rejects
/// the comment outright; otherwise anything uncertain, unresolved, or with
/// low coverage marks it `needs_review`; a clean run is `accepted`.
///
/// A Jev authentication failure (401/403) aborts immediately, since no
/// later call in the same run would fare differently.
pub async fn run_verify(
    jev: &JevClient,
    ai: &ClaudeClient,
    issue: &IssueDoc,
    comment: &Comment,
    citations: &[Citation],
    sources: &[Source],
    opts: &VerifyOptions,
) -> Result<VerifyReport> {
    validate_verify_options(opts)?;

    let item_ref = format!("{}#{}", issue.project, issue.number);
    let comment_ref = CommentRef {
        id: comment.id,
        author: Some(comment.author.clone()),
    };

    if citations.is_empty() || sources.is_empty() {
        return Ok(VerifyReport {
            item_ref,
            url: issue.url.clone(),
            comment: comment_ref,
            verdict: Verdict::NeedsReview,
            reasons: vec!["the comment cites nothing verifiable".to_string()],
            coverage: None,
            sources: Vec::new(),
            statements: Vec::new(),
            models: ReportModels::default(),
            usage: ReportUsage::default(),
        });
    }

    let split = split_statements(ai, &comment.body, citations).await?;
    let ai_model = ai.get_ai_client_metadata().model;

    // Bucket each statement's index by the source it resolves to.
    let source_of: Vec<Option<usize>> = split
        .iter()
        .map(|s| {
            s.cites
                .as_deref()
                .and_then(|c| resolve_cites_index(c, &issue.project, sources))
        })
        .collect();
    let mut by_source: Vec<Vec<usize>> = vec![Vec::new(); sources.len()];
    for (i, src) in source_of.iter().enumerate() {
        if let Some(idx) = src {
            by_source[*idx].push(i);
        }
    }

    let mut jev_model = String::new();
    let mut usage = Usage::default();
    let mut supported: BTreeMap<usize, f64> = BTreeMap::new();
    let mut source_reports = Vec::with_capacity(sources.len());

    for (idx, source) in sources.iter().enumerate() {
        let indices = &by_source[idx];
        let (state, truncated) = truncate_middle(&source.text, opts.max_input_chars);
        if indices.is_empty() {
            // Fetched and grouped, but the splitter attributed nothing to it.
            // Still reported, so it is distinguishable from a citation that
            // never resolved at all.
            source_reports.push(SourceReport {
                source: source.name.clone(),
                items: source.items.clone(),
                truncated,
                error: None,
            });
            continue;
        }
        let questions = indices
            .iter()
            .map(|&i| (format!("s{}", i + 1), statement_question(&split[i].text)))
            .collect();
        let request = SystemOneRequest {
            state: serde_json::Value::String(state),
            model: opts.jev_model.clone(),
            questions,
        };
        match jev.system_one(&request).await {
            Err(err) if is_auth_failure(&err) => {
                return Err(err)
                    .with_context(|| format!("Failed to verify against source {}", source.label));
            }
            Err(err) => {
                let error = format!("{err:#}");
                warn!("Could not verify against source {}: {error}", source.label);
                source_reports.push(SourceReport {
                    source: source.name.clone(),
                    items: source.items.clone(),
                    truncated,
                    error: Some(error),
                });
            }
            Ok(response) => {
                jev_model = response.model;
                usage.input_tokens += response.usage.input_tokens;
                usage.output_tokens += response.usage.output_tokens;
                for &i in indices {
                    if let Some(Answer::Noul { noul }) =
                        response.answers.get(&format!("s{}", i + 1))
                    {
                        supported.insert(i, *noul);
                    }
                }
                source_reports.push(SourceReport {
                    source: source.name.clone(),
                    items: source.items.clone(),
                    truncated,
                    error: None,
                });
            }
        }
    }

    let mut coverage = None;
    let mut coverage_reason = None;
    {
        let state = build_coverage_state(&comment.body, &split);
        let request = SystemOneRequest {
            state: serde_json::Value::String(state),
            model: opts.jev_model.clone(),
            questions: BTreeMap::from([("coverage".to_string(), coverage_question())]),
        };
        match jev.system_one(&request).await {
            Err(err) if is_auth_failure(&err) => {
                return Err(err).context("Failed to check statement coverage");
            }
            Err(err) => {
                let msg = format!("{err:#}");
                warn!("Could not check statement coverage: {msg}");
                coverage_reason = Some(format!("coverage unavailable: {msg}"));
            }
            Ok(response) => {
                jev_model = response.model;
                usage.input_tokens += response.usage.input_tokens;
                usage.output_tokens += response.usage.output_tokens;
                if let Some(Answer::Noul { noul }) = response.answers.get("coverage") {
                    coverage = Some(*noul);
                } else {
                    // Answered, but not with the `coverage` noul we asked
                    // for. The guard did not run, and an unrun guard must
                    // not read as a guard that passed — the same rule the
                    // per-statement path applies to an unanswered statement.
                    warn!("The coverage call returned no usable `coverage` answer");
                    coverage_reason = Some(
                        "coverage unavailable: the reply carried no `coverage` answer".to_string(),
                    );
                }
            }
        }
    }

    let mut statements = Vec::with_capacity(split.len());
    let mut reasons = Vec::new();
    let mut any_rejected = false;
    let mut any_review = false;

    for (i, statement) in split.iter().enumerate() {
        let (source_label, score, reason) = match source_of[i] {
            Some(idx) => {
                let src = &sources[idx];
                let score = supported.get(&i).copied();
                let reason = match score {
                    None => {
                        // The source's call failed, or its answer skipped this
                        // key. Either way the statement is unchecked, which
                        // must not read as verified: without this, one failed
                        // source among several still produced `accepted`.
                        any_review = true;
                        Some(format!(
                            "statement {} has no answer against {}",
                            i + 1,
                            src.label
                        ))
                    }
                    Some(s) if s < opts.reject_below => {
                        any_rejected = true;
                        Some(format!(
                            "statement {} scored {s:.2} (< {:.2}) against {}",
                            i + 1,
                            opts.reject_below,
                            src.label
                        ))
                    }
                    Some(s) if s < opts.supported => {
                        any_review = true;
                        Some(format!(
                            "statement {} scored {s:.2} (uncertain) against {}",
                            i + 1,
                            src.label
                        ))
                    }
                    Some(_) => None,
                };
                (Some(src.label.clone()), score, reason)
            }
            None => match &statement.cites {
                None => (None, None, None),
                // A statement about the judged issue, named rather than left
                // null: rule 6's intent, so treated as rule 6 asks.
                Some(raw) if cites_judged_issue(raw, issue) => (None, None, None),
                Some(raw) => {
                    any_review = true;
                    (
                        Some(raw.clone()),
                        None,
                        Some(format!(
                            "statement {} cites {raw:?}, which could not be resolved",
                            i + 1
                        )),
                    )
                }
            },
        };
        if let Some(msg) = &reason {
            reasons.push(msg.clone());
        }
        statements.push(VerifiedStatement {
            text: statement.text.clone(),
            source: source_label,
            supported: score,
            reason,
        });
    }

    if statements.iter().all(|s| s.supported.is_none()) {
        any_review = true;
        reasons.push("no statement could be checked against a resolved source".to_string());
    }

    match (coverage, &coverage_reason) {
        (Some(cov), _) if cov < opts.coverage => {
            any_review = true;
            reasons.push(format!("coverage scored {cov:.2} (< {:.2})", opts.coverage));
        }
        (None, Some(msg)) => {
            any_review = true;
            reasons.push(msg.clone());
        }
        _ => {}
    }

    let verdict = if any_rejected {
        Verdict::Rejected
    } else if any_review {
        Verdict::NeedsReview
    } else {
        Verdict::Accepted
    };

    Ok(VerifyReport {
        item_ref,
        url: issue.url.clone(),
        comment: comment_ref,
        verdict,
        reasons,
        coverage,
        sources: source_reports,
        statements,
        models: ReportModels {
            jev: jev_model,
            ai: ai_model,
        },
        usage: ReportUsage { jev: usage },
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude::test_utils::ConfigurableMockAiClient;
    use crate::provider::{GitProvider, ItemState};
    use crate::test_support::shim::{retry_on_etxtbsy, shim_lock, write_exec_script};
    use std::path::PathBuf;

    fn judged(number: u64) -> ItemRef {
        ItemRef {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            kind: ItemKind::Issue,
            number,
        }
    }

    // ── resolve_cites_index / cites_judged_issue ───────────────────────

    #[test]
    fn resolve_cites_index_matches_a_sources_keys() {
        let citations = find_citations("see #1614", "rust-works/omni-dev", &judged(1));
        let issue = issue_doc(1614, "Title", "Body.", vec![], vec![]);
        let fetched = BTreeMap::from([(("rust-works/omni-dev".to_string(), 1614), issue)]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        assert_eq!(
            resolve_cites_index("#1614", "rust-works/omni-dev", &sources),
            Some(0)
        );
    }

    /// A `cites` string that names nothing citable (no boundary-safe number)
    /// resolves no index rather than mis-reading it.
    #[test]
    fn resolve_cites_index_is_none_for_an_unparseable_cites_string() {
        let sources = Vec::new();
        assert_eq!(
            resolve_cites_index("#1-overview", "rust-works/omni-dev", &sources),
            None
        );
    }

    #[test]
    fn cites_judged_issue_is_true_only_for_the_judged_number() {
        let issue = doc_with_comments(vec![]);
        assert!(cites_judged_issue("#1779", &issue));
        assert!(!cites_judged_issue("#1614", &issue));
        assert!(!cites_judged_issue("#1-overview", &issue));
    }

    // ── parse_comment_selector / select_comment ───────────────────────

    #[test]
    fn parses_latest_case_insensitively() {
        assert_eq!(
            parse_comment_selector("Latest").unwrap(),
            CommentSelector::Latest
        );
    }

    #[test]
    fn parses_a_numeric_id() {
        assert_eq!(
            parse_comment_selector("123").unwrap(),
            CommentSelector::Id(123)
        );
    }

    #[test]
    fn parses_an_issue_comment_url() {
        let sel = parse_comment_selector(
            "https://github.com/rust-works/omni-dev/issues/1779#issuecomment-5742808381",
        )
        .unwrap();
        assert_eq!(sel, CommentSelector::Id(5_742_808_381));
    }

    #[test]
    fn rejects_garbage_selector() {
        assert!(parse_comment_selector("whenever").is_err());
    }

    /// An `#issuecomment-` suffix whose digits do not parse as `u64` is not
    /// a recognised selector, even though the prefix matched.
    #[test]
    fn rejects_an_issue_comment_url_with_a_non_numeric_id() {
        assert!(parse_comment_selector(
            "https://github.com/rust-works/omni-dev/issues/1779#issuecomment-abc"
        )
        .is_err());
    }

    fn doc_with_comments(comments: Vec<Comment>) -> IssueDoc {
        IssueDoc {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            number: 1779,
            kind: ItemKind::Issue,
            title: "t".to_string(),
            state: ItemState::Open,
            body: "b".to_string(),
            comments,
            closed_by: vec![],
            url: "https://github.com/rust-works/omni-dev/issues/1779".to_string(),
        }
    }

    fn comment(id: u64, author: &str, body: &str) -> Comment {
        Comment {
            author: author.to_string(),
            body: body.to_string(),
            id: Some(id),
        }
    }

    #[test]
    fn select_comment_latest_picks_the_last_comment_that_cites_something() {
        let doc = doc_with_comments(vec![
            comment(1, "alice", "no citation here"),
            comment(2, "bob", "settled by #1614"),
            comment(3, "carol", "thanks!"),
        ]);
        let selected =
            select_comment(&doc, &CommentSelector::Latest, "rust-works/omni-dev").unwrap();
        assert_eq!(selected.id, Some(2));
    }

    #[test]
    fn select_comment_latest_errors_when_none_cite_anything() {
        let doc = doc_with_comments(vec![comment(1, "alice", "thanks!")]);
        let err =
            select_comment(&doc, &CommentSelector::Latest, "rust-works/omni-dev").unwrap_err();
        assert!(err.to_string().contains("--comment ID"), "{err}");
    }

    #[test]
    fn select_comment_by_id_errors_when_missing() {
        let doc = doc_with_comments(vec![comment(1, "alice", "#1614")]);
        let err =
            select_comment(&doc, &CommentSelector::Id(999), "rust-works/omni-dev").unwrap_err();
        assert!(err.to_string().contains("999"), "{err}");
    }

    // ── group_sources / source text ───────────────────────────────────

    fn issue_doc(
        number: u64,
        title: &str,
        body: &str,
        comments: Vec<Comment>,
        closed_by: Vec<ItemRef>,
    ) -> IssueDoc {
        IssueDoc {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            number,
            kind: ItemKind::Issue,
            title: title.to_string(),
            state: ItemState::Closed,
            body: body.to_string(),
            comments,
            closed_by,
            url: format!("https://github.com/rust-works/omni-dev/issues/{number}"),
        }
    }

    fn pr_doc(number: u64, title: &str, body: &str) -> IssueDoc {
        IssueDoc {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            number,
            kind: ItemKind::ChangeRequest,
            title: title.to_string(),
            state: ItemState::Closed,
            body: body.to_string(),
            comments: vec![],
            closed_by: vec![],
            url: format!("https://github.com/rust-works/omni-dev/pull/{number}"),
        }
    }

    fn pr_ref(number: u64) -> ItemRef {
        ItemRef {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            kind: ItemKind::ChangeRequest,
            number,
        }
    }

    /// Mirrors E4's source "A": `("#1614 and PR #1629", 1614, [1629])`.
    #[test]
    fn group_sources_merges_a_cited_issue_with_its_closing_pr() {
        let citations = find_citations("settled by #1614", "rust-works/omni-dev", &judged(1779));
        let issue = issue_doc(
            1614,
            "Sheets MCP tools",
            "Body text.",
            vec![comment(1, "newhoggy", "A human comment.")],
            vec![pr_ref(1629)],
        );
        let pr = pr_doc(1629, "Add drive_sheets_info", "PR body text.");
        let fetched = BTreeMap::from([
            (("rust-works/omni-dev".to_string(), 1614), issue),
            (("rust-works/omni-dev".to_string(), 1629), pr),
        ]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].label, "#1614");
        assert_eq!(sources[0].name, "#1614 and PR #1629");
        assert_eq!(
            sources[0].keys,
            vec![
                ("rust-works/omni-dev".to_string(), 1614),
                ("rust-works/omni-dev".to_string(), 1629)
            ]
        );
        assert_eq!(
            sources[0].text,
            "## SOURCE: #1614 and PR #1629\n\n\
             # Issue #1614: Sheets MCP tools\n\n\
             Body text.\n\
             \n**Comment by newhoggy:**\n\nA human comment.\n\
             \n\n# Pull request #1629: Add drive_sheets_info\n\n\
             PR body text.\n"
        );
    }

    /// A closing PR in a different repository from the judged issue gets the
    /// qualified `PR project#N` label and heading, not the bare `PR #N` form.
    #[test]
    fn group_sources_uses_a_qualified_label_for_a_cross_repo_closing_pr() {
        let citations = find_citations("settled by #1614", "rust-works/omni-dev", &judged(1779));
        let issue = issue_doc(
            1614,
            "Title",
            "Body.",
            vec![],
            vec![ItemRef {
                provider: GitProvider::GitHub,
                project: "other/repo".to_string(),
                kind: ItemKind::ChangeRequest,
                number: 42,
            }],
        );
        let mut pr = pr_doc(42, "Fix", "PR body.");
        pr.project = "other/repo".to_string();
        let fetched = BTreeMap::from([
            (("rust-works/omni-dev".to_string(), 1614), issue),
            (("other/repo".to_string(), 42), pr),
        ]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "#1614 and PR other/repo#42");
        assert!(sources[0]
            .text
            .contains("# Pull request other/repo#42: Fix"));
    }

    /// Mirrors E4's source "E": `("#1237", 1237, [])` — an issue with no
    /// closing PR keeps a bare label.
    #[test]
    fn group_sources_keeps_a_bare_label_when_there_is_no_closing_pr() {
        let citations = find_citations("see #1237", "rust-works/omni-dev", &judged(1));
        let issue = issue_doc(1237, "Title", "Body.", vec![], vec![]);
        let fetched = BTreeMap::from([(("rust-works/omni-dev".to_string(), 1237), issue)]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        assert_eq!(sources[0].name, "#1237");
    }

    #[test]
    fn group_sources_makes_a_standalone_source_for_a_pr_not_closing_any_cited_issue() {
        let citations = find_citations("see PR #999", "rust-works/omni-dev", &judged(1));
        let pr = pr_doc(999, "Some PR", "PR body.");
        let fetched = BTreeMap::from([(("rust-works/omni-dev".to_string(), 999), pr)]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].label, "PR #999");
        assert_eq!(
            sources[0].text,
            "## SOURCE: PR #999\n\n# Pull request #999: Some PR\n\nPR body.\n"
        );
    }

    #[test]
    fn group_sources_uses_a_qualified_label_across_repos() {
        let citations = find_citations("see other/repo#5", "rust-works/omni-dev", &judged(1));
        let mut issue = issue_doc(5, "Cross repo", "Body.", vec![], vec![]);
        issue.project = "other/repo".to_string();
        let fetched = BTreeMap::from([(("other/repo".to_string(), 5), issue)]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        assert_eq!(sources[0].label, "other/repo#5");
    }

    #[test]
    fn group_sources_skips_a_citation_that_never_resolved() {
        let citations = find_citations("see #404", "rust-works/omni-dev", &judged(1));
        let sources = group_sources(&citations, "rust-works/omni-dev", &BTreeMap::new());
        assert!(sources.is_empty());
    }

    // ── question and prompt wording ────────────────────────────────────

    #[test]
    fn statement_question_reproduces_the_tested_wording() {
        let Question::Noul {
            instructions,
            criteria,
        } = statement_question("PR #1629 added a tool.")
        else {
            panic!("expected a noul question");
        };
        assert_eq!(
            instructions,
            "Is the following single STATEMENT supported by the SOURCE text? Answer true only if the source \
             states it or directly shows it. Answer false if the source contradicts it, if the source leaves \
             it open, optional, deferred or merely possible, or if the source does not mention it. STATEMENT: \
             PR #1629 added a tool."
        );
        let criteria = criteria.unwrap();
        assert_eq!(
            criteria.true_means.unwrap(),
            "The source supports the statement"
        );
        assert_eq!(
            criteria.false_means.unwrap(),
            "The source contradicts it, leaves it open, or does not mention it"
        );
    }

    #[test]
    fn coverage_question_reproduces_the_tested_wording() {
        let Question::Noul {
            instructions,
            criteria,
        } = coverage_question()
        else {
            panic!("expected a noul question");
        };
        assert_eq!(
            instructions,
            "Do the numbered STATEMENTS together say everything the COMMENT claims, with nothing \
             added, strengthened or weakened? Answer false if any claim in the comment is missing, if any \
             statement adds something the comment does not say, or if any statement changes how certain or \
             final a claim is (for example \"considered\" becoming \"decided\")."
        );
        let criteria = criteria.unwrap();
        assert_eq!(
            criteria.true_means.unwrap(),
            "The statements faithfully cover the comment"
        );
        assert_eq!(
            criteria.false_means.unwrap(),
            "A claim is missing, added, or changed in certainty"
        );
    }

    #[test]
    fn build_coverage_state_numbers_statements_from_one() {
        let statements = vec![
            SplitStatement {
                text: "First fact.".to_string(),
                cites: None,
            },
            SplitStatement {
                text: "Second fact.".to_string(),
                cites: None,
            },
        ];
        let state = build_coverage_state("The comment body.", &statements);
        assert_eq!(
            state,
            "## COMMENT\n\nThe comment body.\n\n## STATEMENTS\n\n1. First fact.\n2. Second fact.\n"
        );
    }

    #[test]
    fn split_prompt_names_every_citation_and_falls_back_to_none() {
        let citations = find_citations("see #1614 and PR #1629", "rust-works/omni-dev", &judged(1));
        let prompt = build_split_user_prompt("The comment.", &citations);
        assert!(
            prompt.contains("Items the comment cites: #1614, PR #1629"),
            "{prompt}"
        );
        assert!(prompt.contains("COMMENT:\nThe comment."), "{prompt}");

        let prompt = build_split_user_prompt("no citations here", &[]);
        assert!(
            prompt.contains("Items the comment cites: (none)"),
            "{prompt}"
        );
    }

    /// Both additions to #1779's wording earned their place by fixing a
    /// measured failure (see [`build_split_user_prompt`]), so dropping
    /// either one silently regresses the live behaviour.
    #[test]
    fn split_prompt_keeps_the_two_additions_that_fixed_live_failures() {
        let prompt = build_split_user_prompt("The comment.", &[]);
        // Attribution: resolution framing belongs to the judged issue, which
        // is named the way the comment names it, never by number.
        assert!(
            prompt.contains("resolved, settled or answered the judged issue's question"),
            "{prompt}"
        );
        assert!(prompt.contains("do not add its number"), "{prompt}");
        // Reply shape: the schema-less path has nothing else to go on.
        assert!(prompt.contains(r#""statements""#), "{prompt}");
        assert!(prompt.contains(r#""cites""#), "{prompt}");
    }

    #[test]
    fn strip_code_fence_removes_json_and_bare_fences() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
    }

    /// A non-JSON reply (a schema-less backend may not honour the requested
    /// shape exactly) falls back to YAML, fence and all.
    #[test]
    fn parse_split_response_falls_back_to_yaml() {
        let raw =
            "```yaml\nstatements:\n  - text: \"PR #1629 added a tool.\"\n    cites: \"#1614\"\n```";
        let parsed = parse_split_response(raw).unwrap();
        assert_eq!(parsed.statements.len(), 1);
        assert_eq!(parsed.statements[0].text, "PR #1629 added a tool.");
        assert_eq!(parsed.statements[0].cites.as_deref(), Some("#1614"));
    }

    #[test]
    fn parse_split_response_errors_when_neither_json_nor_yaml_parses() {
        let err = parse_split_response("This is not json or yaml.").unwrap_err();
        assert!(
            format!("{err:#}").contains("Failed to parse the statement splitter's response"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn split_statements_rejects_an_empty_reply() {
        let ai = ai_client(vec![Ok(serde_json::json!({"statements": []}).to_string())]);
        let err = split_statements(&ai, "comment", &[]).await.unwrap_err();
        assert!(err.to_string().contains("no statements"), "{err}");
    }

    #[tokio::test]
    async fn split_statements_rejects_more_than_the_statement_limit() {
        let stmts: Vec<serde_json::Value> = (0..=MAX_STATEMENTS)
            .map(|i| serde_json::json!({"text": format!("statement {i}"), "cites": null}))
            .collect();
        let ai = ai_client(vec![Ok(
            serde_json::json!({"statements": stmts}).to_string()
        )]);
        let err = split_statements(&ai, "comment", &[]).await.unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[tokio::test]
    async fn split_statements_rejects_a_blank_statement() {
        let ai = ai_client(vec![Ok(split_response_json(&[("   ", None)]))]);
        let err = split_statements(&ai, "comment", &[]).await.unwrap_err();
        assert!(err.to_string().contains("empty statement"), "{err}");
    }

    // ── validate_verify_options ─────────────────────────────────────────

    fn opts() -> VerifyOptions {
        VerifyOptions {
            jev_model: "jev-latest".to_string(),
            supported: DEFAULT_SUPPORTED,
            reject_below: DEFAULT_REJECT_BELOW,
            coverage: DEFAULT_COVERAGE,
            max_input_chars: 60_000,
        }
    }

    #[test]
    fn rejects_reject_below_greater_than_threshold() {
        let mut o = opts();
        o.reject_below = 0.9;
        o.supported = 0.5;
        assert!(validate_verify_options(&o).is_err());
    }

    #[test]
    fn rejects_out_of_range_knobs() {
        for f in [
            |o: &mut VerifyOptions| o.supported = 1.5,
            |o: &mut VerifyOptions| o.reject_below = -0.1,
            |o: &mut VerifyOptions| o.coverage = 2.0,
            |o: &mut VerifyOptions| o.max_input_chars = 0,
        ] {
            let mut o = opts();
            f(&mut o);
            assert!(validate_verify_options(&o).is_err());
        }
    }

    // ── fetch_verify_input (fake-gh shim) ────────────────────────────────

    fn fake_gh_no_citations(dir: &Path) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let issue = serde_json::json!({
            "title": "t", "body": "b", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 1, "nodes": [
                {"databaseId": 1, "author": {"login": "newhoggy"}, "body": "thanks!"}
            ]},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {issue}}}}}}}\nJSON\n"
            ),
        );
        (path, guard)
    }

    #[test]
    fn fetch_verify_input_short_circuits_when_the_comment_cites_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_no_citations(dir.path());
        let (issue, comment, citations, sources) = retry_on_etxtbsy(|| {
            fetch_verify_input(
                &bin,
                "rust-works/omni-dev",
                &judged(1),
                &CommentSelector::Id(1),
            )
        })
        .unwrap();
        assert_eq!(issue.number, 1);
        assert_eq!(comment.body, "thanks!");
        assert!(citations.is_empty());
        assert!(sources.is_empty());
    }

    /// A cited issue's own closing PR is not among the original citations,
    /// so resolving it takes a second `fetch_items` round.
    fn fake_gh_second_round_pr(dir: &Path) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let issue = serde_json::json!({
            "title": "t", "body": "b", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 1, "nodes": [
                {"databaseId": 1, "author": {"login": "newhoggy"}, "body": "settled by #1614"}
            ]},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let cited_issue = serde_json::json!({
            "__typename": "Issue",
            "title": "cited", "body": "cited body", "state": "CLOSED", "url": "u2",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": [{"number": 1629}]}
        });
        let closing_pr = serde_json::json!({
            "__typename": "PullRequest",
            "title": "closer", "body": "pr body", "state": "MERGED", "url": "u3"
        });
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncase \"$4\" in\n\
                 *'number:1614'*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {cited_issue}}}}}}}\nJSON\n;;\n\
                 *'number:1629'*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {closing_pr}}}}}}}\nJSON\n;;\n\
                 *) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {issue}}}}}}}\nJSON\n;;\n\
                 esac\n",
            ),
        );
        (path, guard)
    }

    #[test]
    fn fetch_verify_input_fetches_a_second_round_of_closing_prs() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_second_round_pr(dir.path());
        let (_issue, _comment, citations, sources) = retry_on_etxtbsy(|| {
            fetch_verify_input(
                &bin,
                "rust-works/omni-dev",
                &judged(1),
                &CommentSelector::Latest,
            )
        })
        .unwrap();
        assert_eq!(citations.len(), 1);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "#1614 and PR #1629");
        assert_eq!(
            sources[0].keys,
            vec![
                ("rust-works/omni-dev".to_string(), 1614),
                ("rust-works/omni-dev".to_string(), 1629)
            ]
        );
    }

    // ── run_verify (wiremock + mock AI client) ───────────────────────────

    fn split_response_json(statements: &[(&str, Option<&str>)]) -> String {
        let stmts: Vec<serde_json::Value> = statements
            .iter()
            .map(|(text, cites)| serde_json::json!({"text": text, "cites": cites}))
            .collect();
        serde_json::json!({"statements": stmts}).to_string()
    }

    fn ai_client(responses: Vec<Result<String>>) -> ClaudeClient {
        ClaudeClient::new(Box::new(ConfigurableMockAiClient::new(responses)))
    }

    fn noul_json(value: f64) -> serde_json::Value {
        serde_json::json!({"type": "noul", "noul": value})
    }

    async fn source_and_coverage_server(
        source_answers: serde_json::Value,
        coverage_noul: f64,
    ) -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        // Coverage call: state starts with "## COMMENT"; source calls: state
        // starts with "## SOURCE:". Distinguish via body_string_contains.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## SOURCE:"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": source_answers,
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## COMMENT"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"coverage": noul_json(coverage_noul)},
                    "usage": {"input_tokens": 3, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        server
    }

    fn issue_and_source() -> (IssueDoc, Comment, Vec<Citation>, Vec<Source>) {
        let issue = doc_with_comments(vec![comment(1, "newhoggy", "settled by #1614")]);
        let citations = find_citations(
            &issue.comments[0].body,
            "rust-works/omni-dev",
            &judged(1779),
        );
        let cited = issue_doc(1614, "Cited issue", "Cited body.", vec![], vec![]);
        let fetched = BTreeMap::from([(("rust-works/omni-dev".to_string(), 1614), cited)]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        (
            issue,
            comment(1, "newhoggy", "settled by #1614"),
            citations,
            sources,
        )
    }

    #[tokio::test]
    async fn run_verify_accepts_a_clean_comment() {
        let server =
            source_and_coverage_server(serde_json::json!({"s1": noul_json(0.95)}), 0.9).await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "PR #1629 exists.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::Accepted);
        assert!(report.reasons.is_empty(), "{:?}", report.reasons);
        assert_eq!(report.statements[0].supported, Some(0.95));
        assert_eq!(report.models.ai, "mock-model");
        assert_eq!(report.usage.jev.input_tokens, 13);
    }

    #[tokio::test]
    async fn run_verify_rejects_when_a_statement_scores_below_reject_below() {
        let server =
            source_and_coverage_server(serde_json::json!({"s1": noul_json(0.05)}), 0.9).await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "A false claim.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::Rejected);
        assert!(
            report.reasons[0].contains("scored 0.05"),
            "{:?}",
            report.reasons
        );
    }

    #[tokio::test]
    async fn run_verify_needs_review_when_a_statement_is_uncertain() {
        let server =
            source_and_coverage_server(serde_json::json!({"s1": noul_json(0.4)}), 0.9).await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "An uncertain claim.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::NeedsReview);
    }

    #[tokio::test]
    async fn run_verify_needs_review_when_coverage_is_low() {
        let server =
            source_and_coverage_server(serde_json::json!({"s1": noul_json(0.95)}), 0.1).await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "A true claim.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::NeedsReview);
        assert!(
            report
                .reasons
                .iter()
                .any(|r| r.contains("coverage scored 0.10")),
            "{:?}",
            report.reasons
        );
    }

    #[tokio::test]
    async fn run_verify_needs_review_when_no_citations_resolved() {
        let jev = JevClient::new("http://127.0.0.1:1", "key").unwrap();
        let ai = ai_client(vec![]);
        let issue = doc_with_comments(vec![comment(1, "newhoggy", "no citation here")]);
        let comment = comment(1, "newhoggy", "no citation here");

        let report = run_verify(&jev, &ai, &issue, &comment, &[], &[], &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::NeedsReview);
        assert_eq!(report.reasons, ["the comment cites nothing verifiable"]);
    }

    /// A statement whose source never answered is unchecked, and an
    /// unchecked statement must not ride along inside an `accepted`
    /// verdict. Before this, one failed source among several still
    /// produced `accepted` — with the unanswered statement listed in
    /// `reasons`, which `accepted` is documented never to carry.
    #[tokio::test]
    async fn run_verify_needs_review_when_only_one_of_two_sources_answers() {
        let server = wiremock::MockServer::start().await;
        // The #1614 source answers; the #1237 source fails outright.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("SOURCE: #1614"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"s1": noul_json(0.95)},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("SOURCE: #1237"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## COMMENT"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"coverage": noul_json(0.9)},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[
            ("A claim about #1614.", Some("#1614")),
            ("A claim about #1237.", Some("#1237")),
        ]))]);

        let issue = doc_with_comments(vec![comment(1, "newhoggy", "see #1614 and #1237")]);
        let comment = comment(1, "newhoggy", "see #1614 and #1237");
        let citations = find_citations(&comment.body, "rust-works/omni-dev", &judged(1779));
        let fetched = BTreeMap::from([
            (
                ("rust-works/omni-dev".to_string(), 1614),
                issue_doc(1614, "One", "Body one.", vec![], vec![]),
            ),
            (
                ("rust-works/omni-dev".to_string(), 1237),
                issue_doc(1237, "Two", "Body two.", vec![], vec![]),
            ),
        ]);
        let sources = group_sources(&citations, "rust-works/omni-dev", &fetched);
        assert_eq!(sources.len(), 2);

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::NeedsReview);
        assert_eq!(report.statements[0].supported, Some(0.95));
        assert!(report.statements[1].supported.is_none());
        assert!(
            report.reasons.iter().any(|r| r.contains("has no answer")),
            "{:?}",
            report.reasons
        );
        // The failed source is still reported, carrying its error.
        assert!(report.sources.iter().any(|s| s.error.is_some()));
    }

    /// A source that resolved but that the splitter attributed nothing to
    /// is still listed, so it is distinguishable from a citation that
    /// never resolved.
    #[tokio::test]
    async fn run_verify_reports_a_source_with_no_attributed_statements() {
        let server = source_and_coverage_server(serde_json::json!({}), 0.9).await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "A decision about this issue.",
            None,
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.sources.len(), 1);
        assert_eq!(report.sources[0].source, "#1614");
        assert!(report.sources[0].error.is_none());
    }

    /// Rule 6 asks the splitter to leave `cites` null for a statement about
    /// the judged issue. When it names the judged issue instead, that is
    /// the rule's intent, not an unresolved citation.
    #[tokio::test]
    async fn run_verify_treats_a_self_citation_as_a_judged_issue_statement() {
        let server =
            source_and_coverage_server(serde_json::json!({"s1": noul_json(0.95)}), 0.9).await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[
            ("A claim about #1614.", Some("#1614")),
            ("This issue's question is settled.", Some("#1779")),
        ]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::Accepted);
        assert!(report.statements[1].source.is_none());
        assert!(report.statements[1].reason.is_none());
        assert!(report.reasons.is_empty(), "{:?}", report.reasons);
    }

    /// With no call made, no model answered, so neither is named.
    #[test]
    fn report_models_omit_the_models_that_never_answered() {
        let value = serde_json::to_value(ReportModels::default()).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    #[tokio::test]
    async fn run_verify_flags_an_unresolved_citation() {
        let server = source_and_coverage_server(serde_json::json!({}), 0.9).await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        // The splitter names a citation that group_sources never produced a
        // source for (e.g. drift between the splitter and the fetch).
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "Some claim.",
            Some("#9999"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();

        assert_eq!(report.verdict, Verdict::NeedsReview);
        assert!(report.statements[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("could not be resolved"));
    }

    #[tokio::test]
    async fn run_verify_stops_on_an_auth_failure() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "A claim.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let err = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("Failed to verify against source #1614"));
    }

    /// An auth failure on the coverage call itself (distinct from a
    /// per-source call) still aborts the whole run.
    #[tokio::test]
    async fn run_verify_stops_on_a_coverage_auth_failure() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## SOURCE:"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"s1": noul_json(0.95)},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## COMMENT"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "A claim.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let err = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("Failed to check statement coverage"),
            "{err:#}"
        );
    }

    /// A non-auth failure on the coverage call is a review reason, not a
    /// fatal error — the per-statement checks already ran.
    #[tokio::test]
    async fn run_verify_needs_review_when_the_coverage_call_fails() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## SOURCE:"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"s1": noul_json(0.95)},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## COMMENT"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "A claim.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();
        assert_eq!(report.verdict, Verdict::NeedsReview);
        assert!(report.coverage.is_none());
        assert!(
            report
                .reasons
                .iter()
                .any(|r| r.contains("coverage unavailable")),
            "{:?}",
            report.reasons
        );
    }

    /// A coverage reply that carries no `coverage` answer must not read as a
    /// guard that ran and passed.
    #[tokio::test]
    async fn run_verify_needs_review_when_the_coverage_answer_is_missing() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## SOURCE:"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"s1": noul_json(0.95)},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("## COMMENT"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        let jev = JevClient::new(&server.uri(), "key").unwrap();
        let ai = ai_client(vec![Ok(split_response_json(&[(
            "A claim.",
            Some("#1614"),
        )]))]);
        let (issue, comment, citations, sources) = issue_and_source();

        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts())
            .await
            .unwrap();
        assert_eq!(report.verdict, Verdict::NeedsReview);
        assert!(report.coverage.is_none());
        assert!(
            report
                .reasons
                .iter()
                .any(|r| r.contains("no `coverage` answer")),
            "{:?}",
            report.reasons
        );
    }
}
