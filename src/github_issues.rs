//! GitHub issue fetching for `ai jev route` (#1779).
//!
//! Mirrors [`crate::pr_status`]'s shape: shell out to `gh` (ADR-0003 — the
//! GitHub token never enters our process), build one aliased `gh api
//! graphql` query per batch of issues grouped by repository (free aliasing —
//! `repository(owner:,name:)` and `issue(number:)` take no `first:`/`last:`,
//! so neither is a connection and neither contributes to the query's point
//! cost — see `pr_status`'s module docs for the measured numbers), and reuse
//! `crate::pr_status::run_gh_graphql` rather than writing a second
//! subprocess wrapper.
//!
//! Every call funnels through [`crate::github_metrics::run_gh`] (or
//! `run_gh_graphql`, which already does), so it is counted and logged like
//! every other `gh` invocation (#1387).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use tracing::{debug, warn};

use crate::provider::{Comment, GitProvider, IssueDoc, ItemKind, ItemRef, ItemState};
use crate::utils::env::EnvSource;

mod cache;

pub use cache::{CacheUsage, IssueCache, DEFAULT_TTL as DEFAULT_CACHE_TTL, GITHUB_CACHE_TTL_ENV};

/// Most issues resolved by one `gh api graphql` call. Each issue carries up
/// to [`MAX_COMMENTS`] comment bodies, so an unbounded `--all-open` query on a
/// large backlog would hit GraphQL's node and response-size limits.
const MAX_ISSUES_PER_QUERY: usize = 25;

/// Most comments fetched per issue: the **latest** ones, since a decision
/// comment lands late and is what moves routing most. An issue with more is
/// warned about, not paginated: the routed input is truncated well before
/// this many comments would fit anyway.
const MAX_COMMENTS: usize = 100;

/// The login GitHub reports for a deleted account.
const GHOST_LOGIN: &str = "ghost";

/// Whether `raw` names an issue only by number (`#N` or `N`), so it needs the
/// current repository to resolve. Lets callers skip the `gh repo view`
/// round-trip when every argument is already qualified.
#[must_use]
pub fn needs_default_project(raw: &str) -> bool {
    let raw = raw.trim();
    let digits = raw.strip_prefix('#').unwrap_or(raw);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Parses one `<ISSUE>` CLI argument into an [`ItemRef`].
///
/// Accepts, in order:
/// - a full GitHub issue URL: `https://github.com/OWNER/REPO/issues/N`
/// - `owner/repo#N`
/// - `#N` or bare `N`, resolved against `default_project` (the `-C/--repo`
///   checkout's own `owner/repo`) — an error naming the missing context if
///   `default_project` is `None`.
pub fn parse_issue_arg(raw: &str, default_project: Option<&str>) -> Result<ItemRef> {
    let raw = raw.trim();

    if let Some(rest) = raw
        .strip_prefix("https://github.com/")
        .or_else(|| raw.strip_prefix("http://github.com/"))
    {
        let mut parts = rest.splitn(4, '/');
        let owner = parts.next().filter(|s| !s.is_empty());
        let repo = parts.next().filter(|s| !s.is_empty());
        let segment = parts.next();
        let number_part = parts.next();
        if let (Some(owner), Some(repo), Some("issues"), Some(number_part)) =
            (owner, repo, segment, number_part)
        {
            let digits: String = number_part
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let number = digits
                .parse::<u64>()
                .map_err(|_| anyhow!("`{raw}` does not end in a valid issue number"))?;
            return Ok(ItemRef {
                provider: GitProvider::GitHub,
                project: format!("{owner}/{repo}"),
                kind: ItemKind::Issue,
                number,
            });
        }
        bail!(
            "`{raw}` is not a recognised GitHub issue URL \
             (expected https://github.com/OWNER/REPO/issues/N)"
        );
    }

    if let Some((project, number_part)) = raw.split_once('#') {
        if project.contains('/') {
            if !is_owner_repo(project) {
                bail!("`{raw}` does not name a repository as owner/repo");
            }
            let number = number_part
                .parse::<u64>()
                .with_context(|| format!("`{raw}` does not have a valid issue number"))?;
            return Ok(ItemRef {
                provider: GitProvider::GitHub,
                project: project.to_string(),
                kind: ItemKind::Issue,
                number,
            });
        }
    }

    let digits = raw.strip_prefix('#').unwrap_or(raw);
    if let Ok(number) = digits.parse::<u64>() {
        let project = default_project.ok_or_else(|| {
            anyhow!(
                "`{raw}` has no repository; pass owner/repo#{digits}, a full issue URL, \
                 or run inside a GitHub repository checkout (see -C/--repo)"
            )
        })?;
        return Ok(ItemRef {
            provider: GitProvider::GitHub,
            project: project.to_string(),
            kind: ItemKind::Issue,
            number,
        });
    }

    bail!(
        "`{raw}` is not a recognised issue reference \
         (expected #N, N, owner/repo#N, or a GitHub issue URL)"
    );
}

/// Whether `project` is exactly `owner/repo`: two non-empty segments with no
/// whitespace. GitHub has no nested paths, so `a/b/c` would otherwise reach
/// the query as a repository literally named `b/c`.
fn is_owner_repo(project: &str) -> bool {
    let segment_ok =
        |s: &str| !s.is_empty() && !s.contains('/') && !s.contains(char::is_whitespace);
    project
        .split_once('/')
        .is_some_and(|(owner, name)| segment_ok(owner) && segment_ok(name))
}

/// Resolves the `owner/repo` of the GitHub repository checked out at `cwd`,
/// via `gh repo view`.
pub fn resolve_current_project(bin: &Path, cwd: &Path) -> Result<String> {
    let output = crate::github_metrics::run_gh(
        bin,
        [
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ],
        "repo view",
        Some(cwd),
    )
    .with_context(|| {
        format!(
            "Failed to run {} (is the GitHub CLI installed?)",
            bin.display()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh repo view failed: {}", stderr.trim());
    }
    let project = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if project.is_empty() {
        bail!("gh repo view returned an empty repository name");
    }
    Ok(project)
}

/// Most issues `--all-open` lists; reaching it is warned about.
const MAX_OPEN_ISSUES: usize = 1000;

/// Lists every open issue number in `project`, for `--all-open`.
pub fn list_open_issue_numbers(bin: &Path, project: &str) -> Result<Vec<u64>> {
    let limit = MAX_OPEN_ISSUES.to_string();
    let output = crate::github_metrics::run_gh(
        bin,
        [
            "issue", "list", "--state", "open", "--json", "number", "-R", project, "--limit",
            &limit,
        ],
        "issue list",
        None,
    )
    .with_context(|| {
        format!(
            "Failed to run {} (is the GitHub CLI installed?)",
            bin.display()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh issue list failed: {}", stderr.trim());
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).context("gh issue list returned invalid JSON")?;
    let numbers: Vec<u64> = value
        .as_array()
        .context("gh issue list returned non-array JSON")?
        .iter()
        .filter_map(|v| v.get("number").and_then(Value::as_u64))
        .collect();
    if numbers.len() >= MAX_OPEN_ISSUES {
        warn!("{project} has at least {MAX_OPEN_ISSUES} open issues; only the first {MAX_OPEN_ISSUES} are listed");
    }
    Ok(numbers)
}

/// Maps a query's `(repo alias index, issue alias index)` back to the
/// [`ItemRef`] it was built for.
type QueryIndex = HashMap<(usize, usize), ItemRef>;

/// Builds the single aliased query for every ref, grouped by project so each
/// repository appears once — same shape as
/// `pr_status::build_query`/`branch_fragment`. Returns `None` for an empty
/// slice.
fn build_issue_query(refs: &[ItemRef]) -> Option<(String, QueryIndex)> {
    if refs.is_empty() {
        return None;
    }
    let mut by_project: BTreeMap<&str, Vec<&ItemRef>> = BTreeMap::new();
    for item_ref in refs {
        by_project
            .entry(item_ref.project.as_str())
            .or_default()
            .push(item_ref);
    }
    let mut index = HashMap::new();
    let mut repos = Vec::new();
    for (ri, (project, items)) in by_project.iter().enumerate() {
        let Some((owner, name)) = project.split_once('/') else {
            continue; // validated by parse_issue_arg; defensive skip only
        };
        let mut frags = Vec::new();
        for (ii, item_ref) in items.iter().enumerate() {
            frags.push(issue_fragment(&format!("i{ii}"), item_ref.number));
            index.insert((ri, ii), (*item_ref).clone());
        }
        let owner = Value::String(owner.to_string());
        let name = Value::String(name.to_string());
        repos.push(format!(
            "r{ri}: repository(owner:{owner}, name:{name}){{\n{}\n}}",
            frags.join("\n")
        ));
    }
    Some((format!("query{{\n{}\n}}", repos.join("\n")), index))
}

/// The GraphQL fragment resolving one issue: title, body, state, url, its
/// latest comments (still returned oldest first, so a routed input reads in
/// conversation order),
/// and the numbers of the change requests that closed it.
/// `closedByPullRequestsReferences` asks only for `number` — the closing PRs'
/// own title/body are out of scope for `route`'s input (the issue explicitly
/// says not to fold referenced items into the routed text); a later
/// `verify-decision` cited-source fetch re-queries them.
fn issue_fragment(alias: &str, number: u64) -> String {
    format!(
        r"{alias}: issue(number:{number}){{
      title body state url
      comments(last:{MAX_COMMENTS}){{ totalCount nodes{{ databaseId author{{ __typename login }} body }} }}
      closedByPullRequestsReferences(first:10){{ nodes{{ number }} }}
    }}"
    )
}

/// Whether a comment author is a bot. GraphQL reports a GitHub App author as
/// `__typename: "Bot"` with the `[bot]` suffix *stripped* from its login
/// (`github-actions`, not `github-actions[bot]`), so the type is the reliable
/// signal; the suffix check covers anything that still carries it.
fn is_bot_author(author: &Value) -> bool {
    author.get("__typename").and_then(Value::as_str) == Some("Bot")
        || author
            .get("login")
            .and_then(Value::as_str)
            .is_some_and(|login| login.ends_with("[bot]"))
}

/// Reads the human comments out of an issue node, warning when the issue has
/// more than [`MAX_COMMENTS`]. A comment whose author was deleted (`author:
/// null`) is kept under [`GHOST_LOGIN`], as GitHub's own UI shows it — the
/// text is still human judgement.
fn human_comments(item_ref: &ItemRef, node: &Value) -> Vec<Comment> {
    let comments = node.get("comments");
    let total = comments
        .and_then(|c| c.get("totalCount"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if total > MAX_COMMENTS as u64 {
        warn!("Issue {item_ref} has {total} comments; only the latest {MAX_COMMENTS} are used");
    }
    comments
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let author = c.get("author").filter(|a| !a.is_null());
            if author.is_some_and(is_bot_author) {
                return None;
            }
            let login = author
                .and_then(|a| a.get("login"))
                .and_then(Value::as_str)
                .unwrap_or(GHOST_LOGIN);
            let body = c.get("body")?.as_str()?.to_string();
            let id = c.get("databaseId").and_then(Value::as_u64);
            Some(Comment {
                author: login.to_string(),
                body,
                id,
            })
        })
        .collect()
}

/// Builds one [`IssueDoc`] from its GraphQL node.
fn build_issue_doc(item_ref: &ItemRef, node: &Value) -> Result<IssueDoc> {
    let title = node
        .get("title")
        .and_then(Value::as_str)
        .with_context(|| format!("issue {item_ref}: response had no `title`"))?
        .to_string();
    let body = node
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = node
        .get("url")
        .and_then(Value::as_str)
        .with_context(|| format!("issue {item_ref}: response had no `url`"))?
        .to_string();
    let state = match node.get("state").and_then(Value::as_str) {
        Some("OPEN") => ItemState::Open,
        Some("CLOSED") => ItemState::Closed,
        other => bail!("issue {item_ref}: unrecognised state {other:?}"),
    };

    let comments = human_comments(item_ref, node);

    let closed_by = node
        .get("closedByPullRequestsReferences")
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|pr| {
            let number = pr.get("number")?.as_u64()?;
            Some(ItemRef {
                provider: GitProvider::GitHub,
                project: item_ref.project.clone(),
                kind: ItemKind::ChangeRequest,
                number,
            })
        })
        .collect();

    Ok(IssueDoc {
        provider: GitProvider::GitHub,
        project: item_ref.project.clone(),
        number: item_ref.number,
        kind: ItemKind::Issue,
        title,
        state,
        body,
        comments,
        closed_by,
        url,
    })
}

/// Reads every aliased issue out of a query reply, keyed by `(project,
/// number)`. Errors, naming the issue, if any aliased node is missing or
/// null — a fetched ref that doesn't resolve is a hard input error here,
/// unlike `pr_status`'s tri-state design (there, "no PR" is a valid
/// successful answer; here, "no such issue" never is).
fn parse_issue_response(
    body: &Value,
    index: &QueryIndex,
) -> Result<HashMap<(String, u64), IssueDoc>> {
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            bail!("{}", describe_graphql_errors(errors, index));
        }
    }
    let data = body
        .get("data")
        .context("gh api graphql response had no `data`")?;

    let mut docs = HashMap::with_capacity(index.len());
    for ((ri, ii), item_ref) in index {
        let node = data
            .get(format!("r{ri}"))
            .and_then(|r| r.get(format!("i{ii}")))
            .filter(|v| !v.is_null());
        let Some(node) = node else {
            bail!("issue {item_ref} was not found (check the number and that gh has access)");
        };
        docs.insert(
            (item_ref.project.clone(), item_ref.number),
            build_issue_doc(item_ref, node)?,
        );
    }
    Ok(docs)
}

/// Turns a GraphQL `errors` array into one message. A `NOT_FOUND` error is
/// mapped back through its `path` (`["r0", "i1"]`) to the issue or
/// repository it names — GitHub reports a missing issue, or a pull-request
/// number asked for as an issue, this way rather than as a `null` node alone.
/// Anything else is reported by its `message`.
fn describe_graphql_errors(errors: &[Value], index: &QueryIndex) -> String {
    let alias = |segment: Option<&Value>, prefix: char| {
        segment
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix(prefix))
            .and_then(|n| n.parse::<usize>().ok())
    };
    let described: Vec<String> = errors
        .iter()
        .map(|error| {
            let path = error.get("path").and_then(Value::as_array);
            let repo = alias(path.and_then(|p| p.first()), 'r');
            let issue = alias(path.and_then(|p| p.get(1)), 'i');
            let not_found = error.get("type").and_then(Value::as_str) == Some("NOT_FOUND");
            match (not_found, repo, issue) {
                (true, Some(ri), Some(ii)) if index.contains_key(&(ri, ii)) => format!(
                    "issue {} was not found (check the number, that it is an issue rather \
                     than a pull request, and that gh has access)",
                    index[&(ri, ii)]
                ),
                (true, Some(ri), None) => {
                    let project = index
                        .iter()
                        .find(|((r, _), _)| *r == ri)
                        .map_or("?", |(_, item_ref)| item_ref.project.as_str());
                    format!("repository {project} was not found (or gh has no access to it)")
                }
                _ => error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| error.to_string(), str::to_string),
            }
        })
        .collect();
    format!("gh api graphql failed: {}", described.join("; "))
}

/// Fetches every ref, returning issues in the same order as `refs`.
///
/// Makes one `gh api graphql` call per `MAX_ISSUES_PER_QUERY` refs, each
/// grouping its refs by repo alias. **Blocking** — callers must be on a
/// blocking thread.
pub fn fetch_issues(bin: &Path, refs: &[ItemRef]) -> Result<Vec<IssueDoc>> {
    let mut by_key = HashMap::with_capacity(refs.len());
    for chunk in refs.chunks(MAX_ISSUES_PER_QUERY) {
        let Some((query, index)) = build_issue_query(chunk) else {
            continue;
        };
        let body = crate::pr_status::run_gh_graphql(bin, &query)?;
        by_key.extend(parse_issue_response(&body, &index)?);
    }
    refs.iter()
        .map(|item_ref| {
            by_key
                .get(&(item_ref.project.clone(), item_ref.number))
                .cloned()
                .ok_or_else(|| anyhow!("issue {item_ref} missing from the parsed gh reply (bug)"))
        })
        .collect()
}

/// The GraphQL fragment resolving one reference that may be either an issue
/// or a pull request — used by `verify-decision` to fetch a decision
/// comment's cited sources, where `#N` is ambiguous until the API answers.
/// `__typename` decides the [`ItemKind`]; only [`ItemKind::Issue`] carries
/// comments and closing PRs (a pull request's own body is the whole source).
fn item_fragment(alias: &str, number: u64) -> String {
    format!(
        r"{alias}: issueOrPullRequest(number:{number}){{
      __typename
      ... on Issue {{
        title body state url
        comments(last:{MAX_COMMENTS}){{ totalCount nodes{{ databaseId author{{ __typename login }} body }} }}
        closedByPullRequestsReferences(first:10){{ nodes{{ number }} }}
      }}
      ... on PullRequest {{
        title body state url
      }}
    }}"
    )
}

/// [`build_issue_query`], but for [`item_fragment`] — same repo-grouping and
/// alias scheme, so [`parse_item_response`] can reuse [`describe_graphql_errors`].
fn build_item_query(refs: &[ItemRef]) -> Option<(String, QueryIndex)> {
    build_aliased_query(refs, item_fragment)
}

/// The GraphQL fragment resolving only an issue's or pull request's `state`,
/// under the same `issueOrPullRequest` field as [`item_fragment`].
fn state_fragment(alias: &str, number: u64) -> String {
    format!(
        r"{alias}: issueOrPullRequest(number:{number}){{
      __typename
      ... on Issue {{ state }}
      ... on PullRequest {{ state }}
    }}"
    )
}

/// [`build_item_query`] for [`state_fragment`].
fn build_state_query(refs: &[ItemRef]) -> Option<(String, QueryIndex)> {
    build_aliased_query(refs, state_fragment)
}

/// Minimal timestamp selection for either item type. The aliases key replies.
fn updated_at_fragment(alias: &str, number: u64) -> String {
    format!(
        r"{alias}: issueOrPullRequest(number:{number}){{
      ... on Issue {{ updatedAt }}
      ... on PullRequest {{ updatedAt }}
    }}"
    )
}

fn build_updated_at_query(refs: &[ItemRef]) -> Option<(String, QueryIndex)> {
    build_aliased_query(refs, updated_at_fragment)
}

fn parse_updated_at_response(
    body: &Value,
    index: &QueryIndex,
) -> Result<Parsed<Option<DateTime<Utc>>>> {
    parse_aliased_response(body, index, |item_ref, node| {
        let timestamp = node
            .get("updatedAt")
            .and_then(Value::as_str)
            .with_context(|| format!("item {item_ref}: response had no string `updatedAt`"))?;
        DateTime::parse_from_rfc3339(timestamp)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .with_context(|| format!("item {item_ref}: invalid `updatedAt` {timestamp:?}"))
    })
}

/// Builds the single aliased query for every ref, grouped by project so each
/// repository appears once, resolving each ref with `fragment`. Returns `None`
/// for an empty slice.
fn build_aliased_query(
    refs: &[ItemRef],
    fragment: fn(&str, u64) -> String,
) -> Option<(String, QueryIndex)> {
    if refs.is_empty() {
        return None;
    }
    let mut by_project: BTreeMap<&str, Vec<&ItemRef>> = BTreeMap::new();
    for item_ref in refs {
        by_project
            .entry(item_ref.project.as_str())
            .or_default()
            .push(item_ref);
    }
    let mut index = HashMap::new();
    let mut repos = Vec::new();
    for (ri, (project, items)) in by_project.iter().enumerate() {
        let Some((owner, name)) = project.split_once('/') else {
            // patchcov: coverage ignore-line reason="validated by the citation parser; every ItemRef reaching here already has a project of the form owner/repo"
            continue;
        };
        let mut frags = Vec::new();
        for (ii, item_ref) in items.iter().enumerate() {
            frags.push(fragment(&format!("i{ii}"), item_ref.number));
            index.insert((ri, ii), (*item_ref).clone());
        }
        let owner = Value::String(owner.to_string());
        let name = Value::String(name.to_string());
        repos.push(format!(
            "r{ri}: repository(owner:{owner}, name:{name}){{\n{}\n}}",
            frags.join("\n")
        ));
    }
    Some((format!("query{{\n{}\n}}", repos.join("\n")), index))
}

/// Reads an `issueOrPullRequest` node's `state`. A merged pull request is
/// closed, as far as `route` is concerned.
fn parse_item_state(item_ref: &ItemRef, node: &Value) -> Result<ItemState> {
    match node.get("state").and_then(Value::as_str) {
        Some("OPEN") => Ok(ItemState::Open),
        Some("CLOSED" | "MERGED") => Ok(ItemState::Closed),
        other => bail!("item {item_ref}: unrecognised state {other:?}"),
    }
}

/// Builds an [`IssueDoc`] from an `issueOrPullRequest` node, using
/// `__typename` — not the caller's `item_ref.kind` hint — to decide whether
/// this is an issue or a change request: a decision comment citing "PR #N"
/// when `#N` is actually an issue (or vice versa) must be resolved by what
/// the item *is*, not by how it was written.
fn build_item_doc(item_ref: &ItemRef, node: &Value) -> Result<IssueDoc> {
    let kind = match node.get("__typename").and_then(Value::as_str) {
        Some("Issue") => ItemKind::Issue,
        Some("PullRequest") => ItemKind::ChangeRequest,
        other => bail!("item {item_ref}: unrecognised __typename {other:?}"),
    };
    let title = node
        .get("title")
        .and_then(Value::as_str)
        .with_context(|| format!("item {item_ref}: response had no `title`"))?
        .to_string();
    let body = node
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = node
        .get("url")
        .and_then(Value::as_str)
        .with_context(|| format!("item {item_ref}: response had no `url`"))?
        .to_string();
    let state = parse_item_state(item_ref, node)?;
    let (comments, closed_by) = if kind == ItemKind::Issue {
        (
            human_comments(item_ref, node),
            node.get("closedByPullRequestsReferences")
                .and_then(|c| c.get("nodes"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|pr| {
                    let number = pr.get("number")?.as_u64()?;
                    Some(ItemRef {
                        provider: GitProvider::GitHub,
                        project: item_ref.project.clone(),
                        kind: ItemKind::ChangeRequest,
                        number,
                    })
                })
                .collect(),
        )
    } else {
        (Vec::new(), Vec::new())
    };

    Ok(IssueDoc {
        provider: GitProvider::GitHub,
        project: item_ref.project.clone(),
        number: item_ref.number,
        kind,
        title,
        state,
        body,
        comments,
        closed_by,
        url,
    })
}

/// The `(missing items, missing repositories)` of [`tolerated_not_found`].
type NotFoundSlots = (HashSet<(usize, usize)>, HashSet<usize>);

/// The aliased slots a reply reports as `NOT_FOUND`: `(repo, item)` pairs for
/// a missing item (path `["rK", "iN"]`) and repo indexes for a missing or
/// inaccessible repository (path `["rK"]`), in that order. Any other GraphQL error (a rate
/// limit, a `NOT_FOUND` on a path the query didn't alias) fails the batch —
/// retrying it per item would not help.
fn tolerated_not_found(body: &Value, index: &QueryIndex) -> Result<NotFoundSlots> {
    let mut not_found = HashSet::new();
    let mut missing_repos = HashSet::new();
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        for error in errors {
            let is_not_found = error.get("type").and_then(Value::as_str) == Some("NOT_FOUND");
            let path = error.get("path").and_then(Value::as_array);
            let alias = |segment: Option<&Value>, prefix: char| {
                segment
                    .and_then(Value::as_str)
                    .and_then(|s| s.strip_prefix(prefix))
                    .and_then(|n| n.parse::<usize>().ok())
            };
            let ri = alias(path.and_then(|p| p.first()), 'r');
            let ii = alias(path.and_then(|p| p.get(1)), 'i');
            match (is_not_found, ri, ii) {
                (true, Some(ri), Some(ii)) if index.contains_key(&(ri, ii)) => {
                    not_found.insert((ri, ii));
                }
                (true, Some(ri), None) if index.keys().any(|(r, _)| *r == ri) => {
                    missing_repos.insert(ri);
                }
                _ => bail!(
                    "{}",
                    describe_graphql_errors(std::slice::from_ref(error), index)
                ),
            }
        }
    }
    Ok((not_found, missing_repos))
}

/// Reads every aliased item out of an [`item_fragment`] query reply.
///
/// Unlike [`parse_issue_response`], a `NOT_FOUND` is **tolerated** as
/// `None` rather than failing the whole batch: a citation can be stale, a
/// typo, or a quoted example (`owner/repo#123`), and one bad citation must
/// not stop `route` or `verify-decision` from handling the rest. That covers
/// both a missing item and a missing or inaccessible repository, which
/// resolves every item cited in it to `None` (#2001); see
/// [`tolerated_not_found`].
fn parse_item_response(
    body: &Value,
    index: &QueryIndex,
) -> Result<HashMap<(String, u64), Option<IssueDoc>>> {
    parse_aliased_response(body, index, build_item_doc)
}

/// [`parse_item_response`] for a [`state_fragment`] reply: `None` for an item
/// GitHub can't find.
fn parse_state_response(
    body: &Value,
    index: &QueryIndex,
) -> Result<HashMap<(String, u64), Option<ItemState>>> {
    parse_aliased_response(body, index, parse_item_state)
}

/// Reads each aliased node out of a reply with `build`, mapping a not-found
/// slot to `None`.
fn parse_aliased_response<T>(
    body: &Value,
    index: &QueryIndex,
    build: impl Fn(&ItemRef, &Value) -> Result<T>,
) -> Result<HashMap<(String, u64), Option<T>>> {
    let (not_found, missing_repos) = tolerated_not_found(body, index)?;
    let data = body
        .get("data")
        .context("gh api graphql response had no `data`")?;

    let mut found = HashMap::with_capacity(index.len());
    for ((ri, ii), item_ref) in index {
        let key = (item_ref.project.clone(), item_ref.number);
        if missing_repos.contains(ri) || not_found.contains(&(*ri, *ii)) {
            found.insert(key, None);
            continue;
        }
        let node = data
            .get(format!("r{ri}"))
            .and_then(|r| r.get(format!("i{ii}")))
            .filter(|v| !v.is_null());
        match node {
            Some(node) => {
                found.insert(key, Some(build(item_ref, node)?));
            }
            None => {
                found.insert(key, None);
            }
        }
    }
    Ok(found)
}

/// Fetches a set of references that may be issues or pull requests.
///
/// Tolerates a per-reference "not found" (`None`) rather than failing the
/// whole call, including a reference into a repository GitHub reports as
/// `NOT_FOUND` — used by `route` and `verify-decision` to resolve citations.
/// Any other error (e.g. an SSO-gated org's `FORBIDDEN`) still fails it. Returns one entry per `refs`, in the same order.
/// **Blocking** — callers must be on a blocking thread.
pub fn fetch_items(bin: &Path, refs: &[ItemRef]) -> Result<Vec<Option<IssueDoc>>> {
    fetch_aliased(bin, refs, build_item_query, parse_item_response)
}

/// Fetches only the `state` of each reference (an issue or a pull request),
/// one entry per `refs` in the same order, `None` for one GitHub can't find.
///
/// The shallow recheck a cached copy's `state` is validated with (#2041): the
/// same aliased batching as [`fetch_items`] but with no text, so it is the
/// cheapest way to learn whether a cached item was closed or reopened since.
/// **Blocking.**
pub fn fetch_states(bin: &Path, refs: &[ItemRef]) -> Result<Vec<Option<ItemState>>> {
    fetch_aliased(bin, refs, build_state_query, parse_state_response)
}

/// Fetches only GitHub's `updatedAt` for each issue or pull request.
///
/// Returns one timestamp per ref in caller order (including duplicates), or
/// `None` for an item or repository GitHub cannot find. Empty input makes no
/// request. Uses the same repository aliases and bounded batches as [`fetch_items`].
/// Other GraphQL, transport, and malformed timestamp errors fail the call.
/// **Blocking** — callers must be on a blocking thread.
///
/// A caller can retain this timestamp alongside a fetched document, then
/// compare for equality on a later check. Fetch full data with [`fetch_issues`]
/// or [`fetch_items`] when the timestamp differs (in either direction) or no
/// baseline exists. Neither `None` nor a failed check certifies a cached copy
/// as fresh. The shallow and full requests are separate, not an atomic snapshot.
/// This helper does not renew [`IssueCache`] entries itself.
///
/// This checks issue-level edits, including state, labels and new comments.
/// Projects-v2 board/column changes live on `ProjectV2Item` and do **not** bump
/// the issue's `updatedAt`: this check cannot establish board-state freshness.
pub fn fetch_updated_at(bin: &Path, refs: &[ItemRef]) -> Result<Vec<Option<DateTime<Utc>>>> {
    fetch_aliased(bin, refs, build_updated_at_query, parse_updated_at_response)
}

/// One reply's parsed items, keyed by `(project, number)`.
type Parsed<T> = HashMap<(String, u64), T>;

/// Runs `refs` through `build` in chunks of [`MAX_ISSUES_PER_QUERY`], parses
/// each reply with `parse`, and returns one entry per `refs`, in order.
fn fetch_aliased<T: Clone>(
    bin: &Path,
    refs: &[ItemRef],
    build: fn(&[ItemRef]) -> Option<(String, QueryIndex)>,
    parse: fn(&Value, &QueryIndex) -> Result<Parsed<T>>,
) -> Result<Vec<T>> {
    let mut by_key = HashMap::with_capacity(refs.len());
    for (query, index) in refs.chunks(MAX_ISSUES_PER_QUERY).filter_map(build) {
        let body = crate::pr_status::run_gh_graphql_with_partial_data(bin, &query)?;
        by_key.extend(parse(&body, &index)?);
    }
    refs.iter()
        .map(|item_ref| {
            by_key
                .get(&(item_ref.project.clone(), item_ref.number))
                .cloned()
                .ok_or_else(|| anyhow!("item {item_ref} missing from the parsed gh reply (bug)"))
        })
        .collect()
}

/// [`fetch_issues`] through `cache`: serves fresh cached issues from disk,
/// fetches only the rest, and caches what it fetched (#1858).
///
/// A cached pull request is not served, so asking for one as an issue still
/// reaches GitHub and fails as it would uncached. **Blocking.**
pub fn fetch_issues_cached(
    bin: &Path,
    cache: &IssueCache,
    refs: &[ItemRef],
) -> Result<Vec<IssueDoc>> {
    fetch_issues_through(bin, cache, refs, |_| true)
}

/// [`fetch_issues_cached`], but a cached issue is served only if its `state`
/// still matches GitHub's (#2041): a mismatch is a miss, fetched in full.
///
/// `state` decides what `route` does (skip, refuse, report a dependency), not
/// just what it says, so it is not trusted for the whole TTL like the text.
/// `listed` is the state the caller already knows every ref has — `--all-open`
/// lists its issues as open — which needs no request; with `None`, the cached
/// refs are rechecked with one shallow [`fetch_states`] call per chunk (none
/// when nothing is cached). A failed recheck is not fatal: the cached copies
/// are then treated as misses. **Blocking.**
pub fn fetch_issues_cached_current(
    bin: &Path,
    cache: &IssueCache,
    refs: &[ItemRef],
    listed: Option<ItemState>,
) -> Result<Vec<IssueDoc>> {
    let check = StateCheck::new(bin, cache, refs, listed, |doc| doc.kind == ItemKind::Issue);
    fetch_issues_through(bin, cache, refs, |doc| check.accepts(cache, doc))
}

fn fetch_issues_through(
    bin: &Path,
    cache: &IssueCache,
    refs: &[ItemRef],
    accept: impl Fn(&IssueDoc) -> bool,
) -> Result<Vec<IssueDoc>> {
    fetch_through_cache(
        refs,
        |item_ref| cache.lookup(item_ref, |doc| doc.kind == ItemKind::Issue && accept(doc)),
        |misses| fetch_issues(bin, misses),
        |doc| cache.store(doc),
    )
}

/// [`fetch_issues`], always from GitHub, writing the result through to `cache`.
///
/// For an issue whose freshness matters to this run but whose copy is still
/// useful to a later cached read. **Blocking.**
pub fn fetch_issues_refreshed(
    bin: &Path,
    cache: &IssueCache,
    refs: &[ItemRef],
) -> Result<Vec<IssueDoc>> {
    let docs = fetch_issues(bin, refs)?;
    for doc in &docs {
        cache.store(doc);
    }
    Ok(docs)
}

/// [`fetch_items`] through `cache`, like [`fetch_issues_cached`]. A
/// not-found item is not cached, so it is re-queried next time. **Blocking.**
pub fn fetch_items_cached(
    bin: &Path,
    cache: &IssueCache,
    refs: &[ItemRef],
) -> Result<Vec<Option<IssueDoc>>> {
    fetch_items_through(bin, cache, refs, |_| true)
}

/// [`fetch_items_cached`] with the `state` check of
/// [`fetch_issues_cached_current`]. **Blocking.**
pub fn fetch_items_cached_current(
    bin: &Path,
    cache: &IssueCache,
    refs: &[ItemRef],
    listed: Option<ItemState>,
) -> Result<Vec<Option<IssueDoc>>> {
    let check = StateCheck::new(bin, cache, refs, listed, |_| true);
    fetch_items_through(bin, cache, refs, |doc| check.accepts(cache, doc))
}

fn fetch_items_through(
    bin: &Path,
    cache: &IssueCache,
    refs: &[ItemRef],
    accept: impl Fn(&IssueDoc) -> bool,
) -> Result<Vec<Option<IssueDoc>>> {
    fetch_through_cache(
        refs,
        |item_ref| cache.lookup(item_ref, &accept).map(Some),
        |misses| fetch_items(bin, misses),
        |doc| {
            if let Some(doc) = doc {
                cache.store(doc);
            }
        },
    )
}

/// How a cached doc's `state` is validated for one call (#2041).
enum StateCheck {
    /// The caller knows every ref's state (`--all-open`'s listing).
    Listed(ItemState),
    /// GitHub's current `state` for each cached ref that was rechecked, keyed
    /// by `(lowercased project, number)`; `None` marks an item GitHub can't
    /// find, or a recheck that failed.
    Rechecked(HashMap<(String, u64), Option<ItemState>>),
}

impl StateCheck {
    /// Checks the refs `cache` could serve (and `kind` accepts) against
    /// `listed`, or against one shallow [`fetch_states`] call for them. Refs
    /// whose state this run already verified, and refs with no entry, need no
    /// call.
    fn new(
        bin: &Path,
        cache: &IssueCache,
        refs: &[ItemRef],
        listed: Option<ItemState>,
        kind: impl Fn(&IssueDoc) -> bool,
    ) -> Self {
        if let Some(state) = listed {
            return Self::Listed(state);
        }
        let cached: Vec<ItemRef> = cache
            .unverified_refs(refs, kind)
            .into_iter()
            .cloned()
            .collect();
        let states = if cached.is_empty() {
            Vec::new()
        } else {
            fetch_states(bin, &cached).unwrap_or_else(|e| {
                warn!("Could not recheck cached GitHub state, refetching it: {e:#}");
                vec![None; cached.len()]
            })
        };
        cache.mark_state_verified(&cached);
        Self::Rechecked(
            cached
                .iter()
                .zip(states)
                .map(|(item_ref, state)| {
                    (
                        (item_ref.project.to_ascii_lowercase(), item_ref.number),
                        state,
                    )
                })
                .collect(),
        )
    }

    /// Whether `doc`'s cached `state` is the one GitHub reported. A doc that
    /// was not rechecked is trusted only if this run verified or fetched it
    /// itself.
    fn accepts(&self, cache: &IssueCache, doc: &IssueDoc) -> bool {
        match self {
            Self::Listed(state) => doc.state == *state,
            Self::Rechecked(current) => {
                match current.get(&(doc.project.to_ascii_lowercase(), doc.number)) {
                    Some(state) => *state == Some(doc.state),
                    None => cache.state_verified(&doc.project, doc.number),
                }
            }
        }
    }
}

/// The lookup / fetch-the-misses / store / restore-caller-order sequence
/// shared by [`fetch_issues_cached`] and [`fetch_items_cached`], which differ
/// only in the slot type (`IssueDoc` or `Option<IssueDoc>`).
///
/// `lookup` serves one ref from the cache, `fetch` fetches exactly the refs
/// that missed (in order), and `store` caches each fetched slot.
fn fetch_through_cache<T>(
    refs: &[ItemRef],
    lookup: impl Fn(&ItemRef) -> Option<T>,
    fetch: impl FnOnce(&[ItemRef]) -> Result<Vec<T>>,
    store: impl Fn(&T),
) -> Result<Vec<T>> {
    let mut found: Vec<Option<T>> = refs.iter().map(lookup).collect();
    let misses = missing_refs(refs, &found);
    if !misses.is_empty() {
        let mut fetched = fetch(&misses)?.into_iter();
        for slot in found.iter_mut().filter(|slot| slot.is_none()) {
            let item = fetched
                .next()
                .context("the fetch returned fewer items than asked for (bug)")?;
            store(&item);
            *slot = Some(item);
        }
    }
    refs.iter()
        .zip(found)
        .map(|(item_ref, item)| {
            item.ok_or_else(|| anyhow!("item {item_ref} missing after the fetch (bug)"))
        })
        .collect()
}

/// The `account_scope` digest of the `gh` login the fetches
/// will run as.
///
/// `None` if `gh` can't say (not installed, not logged in), in which case the
/// cache stays off: an entry could later be served to a different login.
/// **Blocking.**
#[must_use]
pub fn auth_scope(bin: &Path) -> Option<String> {
    match checked_auth_scope(bin) {
        Ok(scope) => Some(scope),
        Err(e) => {
            debug!("No usable gh login for the GitHub fetch cache: {e:#}");
            None
        }
    }
}

/// [`auth_scope`] with the reason it failed. Asks `gh auth token`, which
/// honours `GH_HOST`, `GH_TOKEN` and the active `gh auth switch` account
/// exactly as `gh api` does, and reads local config only (no network). The
/// token is digested at once and never logged.
fn checked_auth_scope(bin: &Path) -> Result<String> {
    let output = crate::github_metrics::run_gh(bin, ["auth", "token"], "auth token", None)
        .with_context(|| format!("Failed to run {} auth token", bin.display()))?;
    if !output.status.success() {
        bail!("gh auth token failed (is `gh` logged in?)");
    }
    let token = String::from_utf8_lossy(&output.stdout);
    let token = token.trim();
    if token.is_empty() {
        bail!("gh auth token printed no token");
    }
    Ok(cache::account_scope(token))
}

/// The cache `route` and `verify-decision` fetch through.
///
/// Configured from `env`, rooted under `base` (`dirs::cache_dir()`),
/// namespaced to the `gh` login (see [`auth_scope`]), and swept of expired
/// entries. **Blocking.**
#[must_use]
pub fn open_cache(
    env: &impl EnvSource,
    base: Option<std::path::PathBuf>,
    bin: &Path,
    refresh: bool,
) -> IssueCache {
    let cache = IssueCache::from_env_with(env, base, auth_scope(bin).as_deref(), refresh);
    cache.prune_expired();
    cache
}

/// [`open_cache`] on a blocking thread, shared by the two commands that
/// fetch through the cache.
pub async fn open_cache_blocking<E>(
    env: E,
    base: Option<std::path::PathBuf>,
    bin: std::path::PathBuf,
    refresh: bool,
) -> Result<std::sync::Arc<IssueCache>>
where
    E: EnvSource + Send + 'static,
{
    tokio::task::spawn_blocking(move || std::sync::Arc::new(open_cache(&env, base, &bin, refresh)))
        .await
        .context("GitHub cache setup task panicked")
}

/// The refs whose `found` slot is still empty, in order.
fn missing_refs<T>(refs: &[ItemRef], found: &[Option<T>]) -> Vec<ItemRef> {
    refs.iter()
        .zip(found)
        .filter(|(_, slot)| slot.is_none())
        .map(|(item_ref, _)| item_ref.clone())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::shim::{retry_on_etxtbsy, shim_lock, write_exec_script};
    use std::path::PathBuf;
    use std::sync::MutexGuard;

    // ── parse_issue_arg ─────────────────────────────────────────────

    #[test]
    fn parses_bare_hash_number_with_default_project() {
        let item_ref = parse_issue_arg("#1779", Some("rust-works/omni-dev")).unwrap();
        assert_eq!(item_ref.project, "rust-works/omni-dev");
        assert_eq!(item_ref.number, 1779);
        assert_eq!(item_ref.kind, ItemKind::Issue);
    }

    #[test]
    fn parses_bare_number_with_default_project() {
        let item_ref = parse_issue_arg("1779", Some("rust-works/omni-dev")).unwrap();
        assert_eq!(item_ref.number, 1779);
    }

    #[test]
    fn bare_number_without_default_project_errors() {
        let err = parse_issue_arg("1779", None).unwrap_err();
        assert!(err.to_string().contains("has no repository"));
    }

    #[test]
    fn parses_owner_repo_hash_number() {
        let item_ref = parse_issue_arg("rust-works/omni-dev#1779", None).unwrap();
        assert_eq!(item_ref.project, "rust-works/omni-dev");
        assert_eq!(item_ref.number, 1779);
    }

    #[test]
    fn owner_repo_hash_number_ignores_default_project() {
        let item_ref = parse_issue_arg("other/repo#42", Some("rust-works/omni-dev")).unwrap();
        assert_eq!(item_ref.project, "other/repo");
        assert_eq!(item_ref.number, 42);
    }

    #[test]
    fn parses_full_issue_url() {
        let item_ref =
            parse_issue_arg("https://github.com/rust-works/omni-dev/issues/1779", None).unwrap();
        assert_eq!(item_ref.project, "rust-works/omni-dev");
        assert_eq!(item_ref.number, 1779);
    }

    #[test]
    fn full_issue_url_ignores_trailing_content() {
        let item_ref = parse_issue_arg(
            "https://github.com/rust-works/omni-dev/issues/1779?tab=comments",
            None,
        )
        .unwrap();
        assert_eq!(item_ref.number, 1779);
    }

    #[test]
    fn rejects_a_pull_request_url() {
        let err =
            parse_issue_arg("https://github.com/rust-works/omni-dev/pull/1779", None).unwrap_err();
        assert!(err
            .to_string()
            .contains("not a recognised GitHub issue URL"));
    }

    #[test]
    fn rejects_garbage_input() {
        let err = parse_issue_arg("not an issue", None).unwrap_err();
        assert!(err.to_string().contains("not a recognised issue reference"));
    }

    #[test]
    fn rejects_a_nested_or_empty_project() {
        for raw in ["a/b/c#1", "/repo#1", "owner/#1", "own er/repo#1"] {
            let err = parse_issue_arg(raw, None).unwrap_err();
            assert!(err.to_string().contains("owner/repo"), "{raw}: {err}");
        }
    }

    #[test]
    fn rejects_non_numeric_hash_number() {
        let err = parse_issue_arg("owner/repo#abc", None).unwrap_err();
        assert!(err.to_string().contains("valid issue number"));
    }

    // ── build_issue_query ───────────────────────────────────────────

    fn item_ref(project: &str, number: u64) -> ItemRef {
        ItemRef {
            provider: GitProvider::GitHub,
            project: project.to_string(),
            kind: ItemKind::Issue,
            number,
        }
    }

    #[test]
    fn build_issue_query_is_none_for_no_refs() {
        assert!(build_issue_query(&[]).is_none());
    }

    #[test]
    fn build_item_query_is_none_for_no_refs() {
        assert!(build_item_query(&[]).is_none());
    }

    #[test]
    fn build_issue_query_groups_by_project() {
        let (query, index) = build_issue_query(&[
            item_ref("rust-works/omni-dev", 1),
            item_ref("rust-works/omni-dev", 2),
            item_ref("other/repo", 3),
        ])
        .unwrap();
        assert_eq!(query.matches("repository(owner:").count(), 2);
        assert_eq!(index.len(), 3);
    }

    // ── fetch_issues (fake-gh shim) ──────────────────────────────────

    fn fake_gh(dir: &Path, stdout: &str, code: i32) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!("#!/bin/sh\ncat <<'JSON'\n{stdout}\nJSON\nexit {code}\n"),
        );
        (path, guard)
    }

    #[test]
    fn fetch_issues_empty_refs_runs_nothing() {
        let docs = fetch_issues(Path::new("/no/such/gh/xyzzy"), &[]).unwrap();
        assert!(docs.is_empty());
    }

    #[test]
    fn fetch_issues_parses_a_single_issue() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {
                    "r0": {
                        "i0": {
                            "title": "Route issues by stage",
                            "body": "Body text",
                            "state": "OPEN",
                            "url": "https://github.com/rust-works/omni-dev/issues/1779",
                            "comments": {"nodes": [
                                {"author": {"login": "newhoggy"}, "body": "A human comment"},
                                {"author": {"login": "github-actions[bot]"}, "body": "drift report"}
                            ]},
                            "closedByPullRequestsReferences": {"nodes": []}
                        }
                    }
                }
            })
            .to_string(),
            0,
        );
        let docs =
            retry_on_etxtbsy(|| fetch_issues(&bin, &[item_ref("rust-works/omni-dev", 1779)]))
                .unwrap();
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert_eq!(doc.title, "Route issues by stage");
        assert_eq!(doc.state, ItemState::Open);
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].author, "newhoggy");
    }

    #[test]
    fn fetch_issues_preserves_caller_order_across_projects() {
        let dir = tempfile::tempdir().unwrap();
        let issue = |title: &str| {
            serde_json::json!({
                "title": title, "body": "", "state": "OPEN",
                "url": "https://example.com", "comments": {"nodes": []},
                "closedByPullRequestsReferences": {"nodes": []}
            })
        };
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {
                    "r0": {"i0": issue("second")},
                    "r1": {"i0": issue("first")},
                }
            })
            .to_string(),
            0,
        );
        // Alphabetical project grouping puts "other/repo" (r0) before
        // "rust-works/omni-dev" (r1), opposite the caller's requested order.
        let refs = [
            item_ref("rust-works/omni-dev", 1),
            item_ref("other/repo", 2),
        ];
        let docs = retry_on_etxtbsy(|| fetch_issues(&bin, &refs)).unwrap();
        assert_eq!(docs[0].title, "first");
        assert_eq!(docs[1].title, "second");
    }

    #[test]
    fn fetch_issues_errors_on_missing_issue() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({"data": {"r0": {"i0": null}}}).to_string(),
            0,
        );
        let err = retry_on_etxtbsy(|| fetch_issues(&bin, &[item_ref("rust-works/omni-dev", 9999)]))
            .unwrap_err();
        assert!(err.to_string().contains("was not found"));
    }

    #[test]
    fn fetch_issues_errors_on_graphql_errors_array() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({"errors": [{"message": "Something broke"}]}).to_string(),
            0,
        );
        let err = retry_on_etxtbsy(|| fetch_issues(&bin, &[item_ref("rust-works/omni-dev", 9999)]))
            .unwrap_err();
        assert_eq!(err.to_string(), "gh api graphql failed: Something broke");
    }

    /// The real API's shape for a missing issue: a `NOT_FOUND` error with a
    /// path, next to a `null` node.
    #[test]
    fn fetch_issues_names_a_missing_issue_from_a_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {"i0": null}},
                "errors": [{
                    "type": "NOT_FOUND", "path": ["r0", "i0"],
                    "message": "Could not resolve to an Issue with the number of 9999."
                }]
            })
            .to_string(),
            0,
        );
        let err = retry_on_etxtbsy(|| fetch_issues(&bin, &[item_ref("rust-works/omni-dev", 9999)]))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("issue rust-works/omni-dev#9999 was not found"),
            "{msg}"
        );
        assert!(msg.contains("pull request"), "{msg}");
    }

    #[test]
    fn fetch_issues_names_a_missing_repository() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": null},
                "errors": [{"type": "NOT_FOUND", "path": ["r0"], "message": "Could not resolve"}]
            })
            .to_string(),
            0,
        );
        let err = retry_on_etxtbsy(|| fetch_issues(&bin, &[item_ref("no/such", 1)])).unwrap_err();
        assert!(
            err.to_string().contains("repository no/such was not found"),
            "{err}"
        );
    }

    #[test]
    fn fetch_issues_filters_bot_comments() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {"i0": {
                    "title": "t", "body": "b", "state": "CLOSED",
                    "url": "https://example.com",
                    "comments": {"nodes": [
                        {"author": {"login": "dependabot[bot]"}, "body": "bump"},
                        {"author": {"login": "human"}, "body": "hi"}
                    ]},
                    "closedByPullRequestsReferences": {"nodes": [{"number": 42}]}
                }}}
            })
            .to_string(),
            0,
        );
        let docs =
            retry_on_etxtbsy(|| fetch_issues(&bin, &[item_ref("rust-works/omni-dev", 1)])).unwrap();
        assert_eq!(docs[0].comments.len(), 1);
        assert_eq!(docs[0].comments[0].author, "human");
        assert_eq!(docs[0].state, ItemState::Closed);
        assert_eq!(docs[0].closed_by.len(), 1);
        assert_eq!(docs[0].closed_by[0].number, 42);
        assert_eq!(docs[0].closed_by[0].kind, ItemKind::ChangeRequest);
    }

    #[test]
    fn fetch_issues_filters_bots_by_typename_and_keeps_deleted_authors() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {"i0": {
                    "title": "t", "body": "b", "state": "OPEN",
                    "url": "https://example.com",
                    "comments": {"totalCount": 3, "nodes": [
                        {"author": {"__typename": "Bot", "login": "github-actions"}, "body": "drift"},
                        {"author": null, "body": "from a deleted account"},
                        {"author": {"__typename": "User", "login": "human"}, "body": "hi"}
                    ]},
                    "closedByPullRequestsReferences": {"nodes": []}
                }}}
            })
            .to_string(),
            0,
        );
        let docs =
            retry_on_etxtbsy(|| fetch_issues(&bin, &[item_ref("rust-works/omni-dev", 1)])).unwrap();
        let authors: Vec<&str> = docs[0].comments.iter().map(|c| c.author.as_str()).collect();
        assert_eq!(authors, ["ghost", "human"]);
    }

    #[test]
    fn fetch_issues_splits_large_batches_across_queries() {
        let dir = tempfile::tempdir().unwrap();
        let issue = serde_json::json!({
            "title": "t", "body": "", "state": "OPEN", "url": "https://example.com",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        // One canned reply answers every chunk: each chunk is a single
        // project, so its aliases are r0/i0..i{n}.
        let repo: serde_json::Map<String, Value> = (0..MAX_ISSUES_PER_QUERY)
            .map(|i| (format!("i{i}"), issue.clone()))
            .collect();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({"data": {"r0": repo}}).to_string(),
            0,
        );
        let refs: Vec<ItemRef> = (1..=MAX_ISSUES_PER_QUERY as u64 + 1)
            .map(|n| item_ref("rust-works/omni-dev", n))
            .collect();
        let docs = retry_on_etxtbsy(|| fetch_issues(&bin, &refs)).unwrap();
        assert_eq!(docs.len(), refs.len());
        assert_eq!(docs.last().unwrap().number, MAX_ISSUES_PER_QUERY as u64 + 1);
    }

    #[test]
    fn issue_fragment_fetches_latest_comments_with_author_type() {
        let fragment = issue_fragment("i0", 7);
        assert!(fragment.contains(&format!("comments(last:{MAX_COMMENTS})")));
        assert!(fragment.contains("totalCount"));
        assert!(fragment.contains("__typename"));
    }

    // ── needs_default_project ────────────────────────────────────────

    #[test]
    fn needs_default_project_only_for_bare_numbers() {
        assert!(needs_default_project("#12"));
        assert!(needs_default_project(" 12 "));
        assert!(!needs_default_project("owner/repo#12"));
        assert!(!needs_default_project(
            "https://github.com/rust-works/omni-dev/issues/12"
        ));
        assert!(!needs_default_project("#"));
        assert!(!needs_default_project("abc"));
    }

    // ── resolve_current_project / list_open_issue_numbers ────────────

    #[test]
    fn resolve_current_project_errors_when_gh_is_missing() {
        let err =
            resolve_current_project(Path::new("/no/such/gh/xyzzy"), Path::new(".")).unwrap_err();
        assert!(err.to_string().contains("Failed to run"));
    }

    #[test]
    fn resolve_current_project_parses_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "rust-works/omni-dev", 0);
        let project = retry_on_etxtbsy(|| resolve_current_project(&bin, dir.path())).unwrap();
        assert_eq!(project, "rust-works/omni-dev");
    }

    #[test]
    fn resolve_current_project_errors_on_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "", 1);
        let err = retry_on_etxtbsy(|| resolve_current_project(&bin, dir.path())).unwrap_err();
        assert!(err.to_string().contains("gh repo view failed"));
    }

    #[test]
    fn list_open_issue_numbers_parses_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!([{"number": 1}, {"number": 2}]).to_string(),
            0,
        );
        let numbers =
            retry_on_etxtbsy(|| list_open_issue_numbers(&bin, "rust-works/omni-dev")).unwrap();
        assert_eq!(numbers, vec![1, 2]);
    }

    #[test]
    fn list_open_issue_numbers_errors_when_gh_is_missing() {
        let err = list_open_issue_numbers(Path::new("/no/such/gh/xyzzy"), "rust-works/omni-dev")
            .unwrap_err();
        assert!(err.to_string().contains("Failed to run"));
    }

    #[test]
    fn list_open_issue_numbers_errors_on_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "", 1);
        let err =
            retry_on_etxtbsy(|| list_open_issue_numbers(&bin, "rust-works/omni-dev")).unwrap_err();
        assert!(err.to_string().contains("gh issue list failed"));
    }

    // ── fetch_items (fake-gh shim) ────────────────────────────────────

    fn pr_ref(project: &str, number: u64) -> ItemRef {
        ItemRef {
            provider: GitProvider::GitHub,
            project: project.to_string(),
            kind: ItemKind::ChangeRequest,
            number,
        }
    }

    #[test]
    fn fetch_items_resolves_an_issue_by_typename() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {"i0": {
                    "__typename": "Issue",
                    "title": "t", "body": "b", "state": "OPEN", "url": "u",
                    "comments": {"totalCount": 1, "nodes": [
                        {"databaseId": 42, "author": {"login": "human"}, "body": "hi"}
                    ]},
                    "closedByPullRequestsReferences": {"nodes": [{"number": 7}]}
                }}}
            })
            .to_string(),
            0,
        );
        let docs = retry_on_etxtbsy(|| fetch_items(&bin, &[item_ref("rust-works/omni-dev", 1614)]))
            .unwrap();
        let doc = docs[0].as_ref().unwrap();
        assert_eq!(doc.kind, ItemKind::Issue);
        assert_eq!(doc.comments[0].id, Some(42));
        assert_eq!(doc.closed_by[0].number, 7);
        assert_eq!(doc.closed_by[0].kind, ItemKind::ChangeRequest);
    }

    #[test]
    fn fetch_items_resolves_a_pull_request_by_typename_even_when_cited_as_an_issue() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {"i0": {
                    "__typename": "PullRequest",
                    "title": "t", "body": "b", "state": "MERGED", "url": "u"
                }}}
            })
            .to_string(),
            0,
        );
        // Cited with an Issue kind hint (e.g. "#1629"); __typename overrides it.
        let docs = retry_on_etxtbsy(|| fetch_items(&bin, &[item_ref("rust-works/omni-dev", 1629)]))
            .unwrap();
        let doc = docs[0].as_ref().unwrap();
        assert_eq!(doc.kind, ItemKind::ChangeRequest);
        assert_eq!(doc.state, ItemState::Closed);
        assert!(doc.comments.is_empty());
    }

    #[test]
    fn fetch_items_tolerates_a_not_found_reference() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {"i0": null}},
                "errors": [{
                    "type": "NOT_FOUND", "path": ["r0", "i0"],
                    "message": "Could not resolve to an issue or pull request."
                }]
            })
            .to_string(),
            0,
        );
        let docs = retry_on_etxtbsy(|| fetch_items(&bin, &[pr_ref("rust-works/omni-dev", 99999)]))
            .unwrap();
        assert!(docs[0].is_none());
    }

    #[test]
    fn fetch_items_keeps_valid_items_when_gh_exits_nonzero_for_one_missing_item() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {
                    "i0": null,
                    "i1": {
                        "__typename": "Issue",
                        "title": "Valid", "body": "b",
                        "state": "OPEN", "url": "u"
                    }
                }},
                "errors": [{
                    "type": "NOT_FOUND", "path": ["r0", "i0"],
                    "message": "Could not resolve to an issue or pull request."
                }]
            })
            .to_string(),
            1,
        );
        let refs = [
            item_ref("rust-works/omni-dev", 2063),
            item_ref("rust-works/omni-dev", 1871),
        ];
        let docs = retry_on_etxtbsy(|| fetch_items(&bin, &refs)).unwrap();
        assert!(docs[0].is_none());
        assert_eq!(docs[1].as_ref().unwrap().title, "Valid");
    }

    /// #2001: a repository-level `NOT_FOUND` (a quoted `owner/repo#123`)
    /// resolves every item cited in that repository to `None`.
    #[test]
    fn fetch_items_tolerates_a_missing_repository() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": null},
                "errors": [{"type": "NOT_FOUND", "path": ["r0"], "message": "Could not resolve"}]
            })
            .to_string(),
            1,
        );
        let refs = [pr_ref("no/such", 1), item_ref("no/such", 2)];
        let docs = retry_on_etxtbsy(|| fetch_items(&bin, &refs)).unwrap();
        assert!(docs.iter().all(Option::is_none), "{docs:?}");
    }

    /// A missing repository doesn't take down items in a repository that
    /// exists: the partial-data reply's valid node still resolves.
    #[test]
    fn fetch_items_keeps_items_in_other_repositories_when_one_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        // `build_item_query` aliases projects in sorted order, so `no/such`
        // is `r0` and `rust-works/omni-dev` is `r1`.
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {
                    "r0": null,
                    "r1": {"i0": {
                        "__typename": "Issue",
                        "title": "Valid", "body": "b",
                        "state": "OPEN", "url": "u"
                    }}
                },
                "errors": [{"type": "NOT_FOUND", "path": ["r0"], "message": "Could not resolve"}]
            })
            .to_string(),
            1,
        );
        let refs = [
            item_ref("rust-works/omni-dev", 1871),
            item_ref("no/such", 123),
        ];
        let docs = retry_on_etxtbsy(|| fetch_items(&bin, &refs)).unwrap();
        assert_eq!(docs[0].as_ref().unwrap().title, "Valid");
        assert!(docs[1].is_none());
    }

    /// A repository-level `NOT_FOUND` on an alias this query never issued
    /// isn't a citation miss we can attribute, so it still fails the batch.
    #[test]
    fn fetch_items_bails_on_a_not_found_for_an_unknown_repository_alias() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({
                "data": {"r0": {"i0": null}},
                "errors": [{"type": "NOT_FOUND", "path": ["r7"], "message": "Could not resolve"}]
            })
            .to_string(),
            1,
        );
        let err = retry_on_etxtbsy(|| fetch_items(&bin, &[item_ref("rust-works/omni-dev", 1)]))
            .unwrap_err();
        assert!(err.to_string().contains("was not found"), "{err}");
    }

    #[test]
    fn item_fragment_uses_the_polymorphic_field() {
        let fragment = item_fragment("i0", 42);
        assert!(fragment.contains("issueOrPullRequest(number:42)"));
        assert!(fragment.contains("__typename"));
        assert!(fragment.contains("... on Issue"));
        assert!(fragment.contains("... on PullRequest"));
    }

    // ── build_item_doc ─────────────────────────────────────────────────

    #[test]
    fn build_item_doc_rejects_an_unrecognised_typename() {
        let node = serde_json::json!({
            "__typename": "Gist", "title": "t", "body": "b", "state": "OPEN", "url": "u"
        });
        let err = build_item_doc(&item_ref("rust-works/omni-dev", 1), &node).unwrap_err();
        assert!(err.to_string().contains("unrecognised __typename"), "{err}");
    }

    #[test]
    fn build_item_doc_rejects_an_unrecognised_state() {
        let node = serde_json::json!({
            "__typename": "Issue", "title": "t", "body": "b", "state": "DRAFT", "url": "u"
        });
        let err = build_item_doc(&item_ref("rust-works/omni-dev", 1), &node).unwrap_err();
        assert!(err.to_string().contains("unrecognised state"), "{err}");
    }

    /// A node missing from the reply with no accompanying GraphQL error
    /// (rather than an explicit `NOT_FOUND`) is still treated as not found.
    #[test]
    fn fetch_items_treats_a_silently_missing_node_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            &serde_json::json!({"data": {"r0": {"i0": null}}}).to_string(),
            0,
        );
        let docs = retry_on_etxtbsy(|| fetch_items(&bin, &[item_ref("rust-works/omni-dev", 1614)]))
            .unwrap();
        assert!(docs[0].is_none());
    }

    // ── fetch_*_cached (fake-gh shim + on-disk cache) ─────────────────

    /// A fake `gh` that appends a line to `<dir>/calls` per invocation, so a
    /// test can assert how many times the network was reached.
    fn counting_gh(dir: &Path, stdout: &str, code: i32) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let path = dir.join("fake-gh");
        let calls = dir.join("calls");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\necho call >> '{}'\ncat <<'JSON'\n{stdout}\nJSON\nexit {code}\n",
                calls.display()
            ),
        );
        (path, guard)
    }

    fn gh_calls(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("calls")).map_or(0, |s| s.lines().count())
    }

    fn one_issue_reply(title: &str) -> String {
        serde_json::json!({"data": {"r0": {"i0": {
            "__typename": "Issue",
            "title": title, "body": "b", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        }}}})
        .to_string()
    }

    #[test]
    fn fetch_issues_cached_reuses_a_fresh_fetch_without_calling_gh() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let (bin, _shim) = counting_gh(dir.path(), &one_issue_reply("first"), 0);
        let refs = [item_ref("rust-works/omni-dev", 1)];

        let first = retry_on_etxtbsy(|| fetch_issues_cached(&bin, &cache, &refs)).unwrap();
        assert_eq!(gh_calls(dir.path()), 1);
        assert!(cache.reuse_note().is_none());

        // A later run: a new cache instance over the same directory.
        let later = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let second = retry_on_etxtbsy(|| fetch_issues_cached(&bin, &later, &refs)).unwrap();
        assert_eq!(
            gh_calls(dir.path()),
            1,
            "the second fetch must be served from disk"
        );
        assert_eq!(first, second);
        assert!(later.reuse_note().is_some());
    }

    #[test]
    fn fetch_issues_cached_fetches_only_misses_and_keeps_caller_order() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        {
            let (bin, _shim) = counting_gh(dir.path(), &one_issue_reply("cached"), 0);
            retry_on_etxtbsy(|| {
                fetch_issues_cached(&bin, &cache, &[item_ref("rust-works/omni-dev", 1)])
            })
            .unwrap();
        }
        let (bin, _shim) = counting_gh(dir.path(), &one_issue_reply("fresh"), 0);
        let refs = [
            item_ref("rust-works/omni-dev", 2),
            item_ref("rust-works/omni-dev", 1),
        ];
        let docs = retry_on_etxtbsy(|| fetch_issues_cached(&bin, &cache, &refs)).unwrap();
        assert_eq!(gh_calls(dir.path()), 2);
        assert_eq!((docs[0].number, docs[0].title.as_str()), (2, "fresh"));
        assert_eq!((docs[1].number, docs[1].title.as_str()), (1, "cached"));
    }

    #[test]
    fn refresh_bypasses_the_cache_for_fetch_issues_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = counting_gh(dir.path(), &one_issue_reply("t"), 0);
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let normal = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        retry_on_etxtbsy(|| fetch_issues_cached(&bin, &normal, &refs)).unwrap();
        let refresh = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, true);
        retry_on_etxtbsy(|| fetch_issues_cached(&bin, &refresh, &refs)).unwrap();
        assert_eq!(gh_calls(dir.path()), 2);
    }

    #[test]
    fn fetch_items_cached_shares_entries_with_fetch_issues_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let (bin, _shim) = counting_gh(dir.path(), &one_issue_reply("t"), 0);
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let item = retry_on_etxtbsy(|| fetch_items_cached(&bin, &cache, &refs)).unwrap();
        let issue = retry_on_etxtbsy(|| fetch_issues_cached(&bin, &cache, &refs)).unwrap();
        assert_eq!(gh_calls(dir.path()), 1);
        assert_eq!(item[0].as_ref(), Some(&issue[0]));
    }

    #[test]
    fn a_cached_pull_request_is_not_served_as_an_issue() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let reply = serde_json::json!({"data": {"r0": {"i0": {
            "__typename": "PullRequest",
            "title": "a PR", "body": "b", "state": "MERGED", "url": "u"
        }}}})
        .to_string();
        let (bin, _shim) = counting_gh(dir.path(), &reply, 0);
        let refs = [item_ref("rust-works/omni-dev", 5)];
        retry_on_etxtbsy(|| fetch_items_cached(&bin, &cache, &refs)).unwrap();
        // Uncached, the issue query fails on a pull request (here, on its
        // `MERGED` state); it must still reach `gh` and fail the same way.
        assert!(retry_on_etxtbsy(|| fetch_issues_cached(&bin, &cache, &refs)).is_err());
        assert_eq!(gh_calls(dir.path()), 2);
    }

    #[test]
    fn fetch_items_cached_does_not_cache_a_not_found_item() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let reply = serde_json::json!({
            "data": {"r0": {"i0": null}},
            "errors": [{"type": "NOT_FOUND", "path": ["r0", "i0"], "message": "nope"}]
        })
        .to_string();
        let (bin, _shim) = counting_gh(dir.path(), &reply, 1);
        let refs = [item_ref("rust-works/omni-dev", 9)];
        for _ in 0..2 {
            let docs = retry_on_etxtbsy(|| fetch_items_cached(&bin, &cache, &refs)).unwrap();
            assert!(docs[0].is_none());
        }
        assert_eq!(gh_calls(dir.path()), 2);
    }

    #[test]
    fn fetch_issues_refreshed_always_calls_gh_and_writes_through() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let (bin, _shim) = counting_gh(dir.path(), &one_issue_reply("t"), 0);
        let refs = [item_ref("rust-works/omni-dev", 1)];
        retry_on_etxtbsy(|| fetch_issues_refreshed(&bin, &cache, &refs)).unwrap();
        retry_on_etxtbsy(|| fetch_issues_refreshed(&bin, &cache, &refs)).unwrap();
        assert_eq!(gh_calls(dir.path()), 2);
        let later = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        retry_on_etxtbsy(|| fetch_issues_cached(&bin, &later, &refs)).unwrap();
        assert_eq!(
            gh_calls(dir.path()),
            2,
            "the refreshed copy is reused later"
        );
    }

    fn cached_doc(project: &str, number: u64) -> IssueDoc {
        IssueDoc {
            provider: GitProvider::GitHub,
            project: project.to_string(),
            number,
            kind: ItemKind::Issue,
            title: "t".to_string(),
            state: ItemState::Open,
            body: "b".to_string(),
            comments: Vec::new(),
            closed_by: Vec::new(),
            url: "u".to_string(),
        }
    }

    // ── updatedAt recheck (#1815) ───────────────────────────────────

    #[test]
    fn updated_at_query_groups_repositories_and_selects_only_timestamps() {
        assert!(build_updated_at_query(&[]).is_none());
        let (query, index) =
            build_updated_at_query(&[item_ref("a/b", 1), item_ref("c/d", 2), item_ref("a/b", 3)])
                .unwrap();
        assert_eq!(index.len(), 3);
        let expected = r#"query{
r0: repository(owner:"a", name:"b"){
i0: issueOrPullRequest(number:1){
      ... on Issue { updatedAt }
      ... on PullRequest { updatedAt }
    }
i1: issueOrPullRequest(number:3){
      ... on Issue { updatedAt }
      ... on PullRequest { updatedAt }
    }
}
r1: repository(owner:"c", name:"d"){
i0: issueOrPullRequest(number:2){
      ... on Issue { updatedAt }
      ... on PullRequest { updatedAt }
    }
}
}"#;
        assert_eq!(query, expected);
        assert_eq!(index[&(0, 1)].number, 3);
        assert_eq!(index[&(1, 0)].project, "c/d");
    }

    #[test]
    fn fetch_updated_at_preserves_order_duplicates_and_normalizes_offsets() {
        assert!(fetch_updated_at(Path::new("/no/such/gh"), &[])
            .unwrap()
            .is_empty());
        let dir = tempfile::tempdir().unwrap();
        let reply = serde_json::json!({"data": {
            "r0": {"i0": {"updatedAt": "2026-10-02T10:00:00+10:00"}},
            "r1": {"i0": {"updatedAt": "2026-10-01T00:00:00Z"},
                   "i1": {"updatedAt": "2026-10-01T00:00:00Z"}}
        }})
        .to_string();
        let (bin, _shim) = fake_gh(dir.path(), &reply, 0);
        let mut pr = item_ref("a/b", 2);
        pr.kind = ItemKind::ChangeRequest;
        let refs = [item_ref("c/d", 1), pr, item_ref("c/d", 1)];
        let stamps = retry_on_etxtbsy(|| fetch_updated_at(&bin, &refs)).unwrap();
        let utc = |s: &str| Some(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc));
        assert_eq!(
            stamps,
            [
                utc("2026-10-01T00:00:00Z"),
                utc("2026-10-02T00:00:00Z"),
                utc("2026-10-01T00:00:00Z")
            ]
        );
    }

    #[test]
    fn updated_at_parser_rejects_invalid_timestamps_with_item_context() {
        let (_, index) = build_updated_at_query(&[item_ref("a/b", 42)]).unwrap();
        for node in [
            serde_json::json!({}),
            serde_json::json!({"updatedAt": null}),
            serde_json::json!({"updatedAt": 123}),
            serde_json::json!({"updatedAt": "yesterday"}),
        ] {
            let err = parse_updated_at_response(
                &serde_json::json!({"data": {"r0": {"i0": node}}}),
                &index,
            )
            .unwrap_err();
            assert!(err.to_string().contains("a/b#42"), "{err}");
            assert!(err.to_string().contains("updatedAt"), "{err}");
        }
    }

    #[test]
    fn updated_at_parser_handles_missing_items_repositories_and_errors() {
        let (_, index) = build_updated_at_query(&[item_ref("a/b", 1)]).unwrap();
        for path in [serde_json::json!(["r0", "i0"]), serde_json::json!(["r0"])] {
            let body = serde_json::json!({"data": {"r0": null}, "errors": [{"type": "NOT_FOUND", "path": path}]});
            assert_eq!(
                parse_updated_at_response(&body, &index).unwrap()[&("a/b".to_string(), 1)],
                None
            );
        }
        let null = serde_json::json!({"data": {"r0": {"i0": null}}});
        assert_eq!(
            parse_updated_at_response(&null, &index).unwrap()[&("a/b".to_string(), 1)],
            None
        );
        for error in [
            serde_json::json!({"type": "RATE_LIMITED", "message": "slow down"}),
            serde_json::json!({"type": "NOT_FOUND", "path": ["r9"]}),
        ] {
            assert!(parse_updated_at_response(
                &serde_json::json!({"data": {}, "errors": [error]}),
                &index
            )
            .is_err());
        }
        assert!(parse_updated_at_response(&serde_json::json!({}), &index).is_err());
    }

    #[test]
    fn fetch_updated_at_chunks_requests_and_propagates_subprocess_errors() {
        let dir = tempfile::tempdir().unwrap();
        let nodes: serde_json::Map<String, Value> = (0..MAX_ISSUES_PER_QUERY)
            .map(|i| {
                (
                    format!("i{i}"),
                    serde_json::json!({"updatedAt": "2026-10-01T00:00:00Z"}),
                )
            })
            .collect();
        let reply = serde_json::json!({"data": {"r0": nodes}}).to_string();
        let refs: Vec<_> = (1..=26).map(|n| item_ref("a/b", n)).collect();
        {
            let (bin, _shim) = counting_gh(dir.path(), &reply, 0);
            let stamps = retry_on_etxtbsy(|| fetch_updated_at(&bin, &refs)).unwrap();
            assert_eq!(stamps.len(), refs.len());
            assert!(stamps.iter().all(Option::is_some));
            assert_eq!(gh_calls(dir.path()), 2);
        }
        let (bin, _shim) = fake_gh(dir.path(), "unavailable", 1);
        assert!(retry_on_etxtbsy(|| fetch_updated_at(&bin, &refs)).is_err());
    }

    // ── state recheck (#2041) ────────────────────────────────────────

    /// A fake `gh` answering a `state`-only query with `state_reply` and any
    /// other query with `full_reply`, logging `state` or `full` per call.
    fn state_gh(
        dir: &Path,
        state_reply: &str,
        full_reply: &str,
    ) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let path = dir.join("fake-gh");
        let calls = dir.join("kinds");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncase \"$4\" in\n\
                 *'on Issue {{ state }}'*) echo state >> '{calls}'; cat <<'JSON'\n{state_reply}\nJSON\n;;\n\
                 *) echo full >> '{calls}'; cat <<'JSON'\n{full_reply}\nJSON\n;;\n\
                 esac\n",
                calls = calls.display()
            ),
        );
        // Production code under test degrades a failed state recheck to a
        // refetch (#2041), which swallows an `ETXTBSY` from the first exec of
        // a freshly written shim and drops the expected `state` call. Exec it
        // once here, where `retry_on_etxtbsy` can see the error, then reset
        // the log.
        retry_on_etxtbsy(|| {
            std::process::Command::new(&path)
                .output()
                .map(drop)
                .map_err(Into::into)
        })
        .unwrap();
        std::fs::remove_file(&calls).unwrap();
        (path, guard)
    }

    fn kinds(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("kinds"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn state_reply(state: &str) -> String {
        serde_json::json!({"data": {"r0": {"i0": {"__typename": "Issue", "state": state}}}})
            .to_string()
    }

    fn issue_reply(state: &str) -> String {
        serde_json::json!({"data": {"r0": {"i0": {
            "__typename": "Issue",
            "title": "fresh", "body": "b", "state": state, "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        }}}})
        .to_string()
    }

    fn cache_holding(cache_dir: &Path, state: ItemState) -> IssueCache {
        let cache = IssueCache::new(cache_dir.to_path_buf(), DEFAULT_CACHE_TTL, false);
        let mut doc = cached_doc("rust-works/omni-dev", 1);
        doc.title = "cached".to_string();
        doc.state = state;
        cache.store(&doc);
        // A later run: the entry is on disk, but this instance did not store it.
        IssueCache::new(cache_dir.to_path_buf(), DEFAULT_CACHE_TTL, false)
    }

    #[test]
    fn build_state_query_is_none_for_no_refs() {
        assert!(build_state_query(&[]).is_none());
    }

    #[test]
    fn build_state_query_asks_only_for_state_and_groups_by_project() {
        let (query, index) = build_state_query(&[
            item_ref("rust-works/omni-dev", 1),
            item_ref("rust-works/omni-dev", 2),
            item_ref("other/repo", 3),
        ])
        .unwrap();
        assert_eq!(query.matches("repository(owner:").count(), 2);
        assert_eq!(index.len(), 3);
        assert!(query.contains("... on Issue { state }"), "{query}");
        assert!(query.contains("... on PullRequest { state }"), "{query}");
        for text in ["title", "body", "comments"] {
            assert!(!query.contains(text), "{text} in {query}");
        }
    }

    #[test]
    fn fetch_states_empty_refs_runs_nothing() {
        let states = fetch_states(Path::new("/no/such/gh/xyzzy"), &[]).unwrap();
        assert!(states.is_empty());
    }

    #[test]
    fn fetch_states_maps_open_closed_merged_and_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let reply = serde_json::json!({
            "data": {"r0": {
                "i0": {"__typename": "Issue", "state": "OPEN"},
                "i1": {"__typename": "Issue", "state": "CLOSED"},
                "i2": {"__typename": "PullRequest", "state": "MERGED"},
                "i3": null
            }},
            "errors": [{"type": "NOT_FOUND", "path": ["r0", "i3"], "message": "nope"}]
        })
        .to_string();
        let (bin, _shim) = fake_gh(dir.path(), &reply, 1);
        let refs: Vec<ItemRef> = (1..=4)
            .map(|n| item_ref("rust-works/omni-dev", n))
            .collect();
        let states = retry_on_etxtbsy(|| fetch_states(&bin, &refs)).unwrap();
        assert_eq!(
            states,
            [
                Some(ItemState::Open),
                Some(ItemState::Closed),
                Some(ItemState::Closed),
                None
            ]
        );
    }

    #[test]
    fn fetch_states_reports_a_missing_repository_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let reply = serde_json::json!({
            "data": {"r0": null},
            "errors": [{"type": "NOT_FOUND", "path": ["r0"], "message": "nope"}]
        })
        .to_string();
        let (bin, _shim) = fake_gh(dir.path(), &reply, 1);
        let states = retry_on_etxtbsy(|| fetch_states(&bin, &[item_ref("gone/repo", 1)])).unwrap();
        assert_eq!(states, [None]);
    }

    #[test]
    fn fetch_states_fails_on_any_other_graphql_error() {
        let dir = tempfile::tempdir().unwrap();
        let reply = serde_json::json!({
            "data": {"r0": null},
            "errors": [{"type": "RATE_LIMITED", "message": "slow down"}]
        })
        .to_string();
        let (bin, _shim) = fake_gh(dir.path(), &reply, 1);
        let refs = [item_ref("rust-works/omni-dev", 1)];
        assert!(retry_on_etxtbsy(|| fetch_states(&bin, &refs)).is_err());
    }

    #[test]
    fn fetch_states_rejects_an_unrecognised_state() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), &state_reply("DRAFT"), 0);
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let err = retry_on_etxtbsy(|| fetch_states(&bin, &refs)).unwrap_err();
        assert!(err.to_string().contains("unrecognised state"), "{err}");
    }

    #[test]
    fn a_cached_issue_whose_state_is_unchanged_is_served_after_one_shallow_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Open);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("OPEN"), &issue_reply("OPEN"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let docs =
            retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert_eq!(docs[0].title, "cached");
        assert_eq!(kinds(dir.path()), ["state"]);
        assert!(cache.reuse_note().is_some());
    }

    #[test]
    fn a_cached_open_issue_closed_since_is_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Open);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("CLOSED"), &issue_reply("CLOSED"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let docs =
            retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert_eq!(
            (docs[0].title.as_str(), docs[0].state),
            ("fresh", ItemState::Closed)
        );
        assert_eq!(kinds(dir.path()), ["state", "full"]);
        assert!(cache.reuse_note().is_none(), "a refetch is not a reuse");

        // The refetched copy was written back.
        let later = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let docs =
            retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &later, &refs, None)).unwrap();
        assert_eq!(docs[0].state, ItemState::Closed);
        assert_eq!(kinds(dir.path()), ["state", "full", "state"]);
    }

    #[test]
    fn a_listed_open_issue_cached_as_closed_is_refetched_without_a_state_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Closed);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("OPEN"), &issue_reply("OPEN"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let docs = retry_on_etxtbsy(|| {
            fetch_issues_cached_current(&bin, &cache, &refs, Some(ItemState::Open))
        })
        .unwrap();
        assert_eq!(docs[0].state, ItemState::Open);
        assert_eq!(kinds(dir.path()), ["full"]);
    }

    #[test]
    fn a_listed_open_issue_cached_as_open_costs_no_call_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Open);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("OPEN"), &issue_reply("OPEN"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let docs = retry_on_etxtbsy(|| {
            fetch_issues_cached_current(&bin, &cache, &refs, Some(ItemState::Open))
        })
        .unwrap();
        assert_eq!(docs[0].title, "cached");
        assert!(kinds(dir.path()).is_empty());
    }

    #[test]
    fn a_cold_cache_makes_no_state_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("OPEN"), &issue_reply("OPEN"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert_eq!(kinds(dir.path()), ["full"]);
        // The copy this run stored is as fresh as a fetch: no recheck either.
        retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert_eq!(kinds(dir.path()), ["full"]);
    }

    #[test]
    fn refresh_makes_no_state_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        cache_holding(cache_dir.path(), ItemState::Open);
        let refresh = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, true);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("OPEN"), &issue_reply("OPEN"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &refresh, &refs, None)).unwrap();
        assert_eq!(kinds(dir.path()), ["full"]);
    }

    #[test]
    fn a_cached_item_github_cannot_find_is_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Open);
        let gone = serde_json::json!({
            "data": {"r0": {"i0": null}},
            "errors": [{"type": "NOT_FOUND", "path": ["r0", "i0"], "message": "nope"}]
        })
        .to_string();
        let (bin, _shim) = state_gh(dir.path(), &gone, &gone);
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let docs =
            retry_on_etxtbsy(|| fetch_items_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert!(docs[0].is_none());
        assert_eq!(kinds(dir.path()), ["state", "full"]);
    }

    #[test]
    fn fetch_items_cached_current_refetches_a_cached_item_closed_since() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Open);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("CLOSED"), &issue_reply("CLOSED"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let docs =
            retry_on_etxtbsy(|| fetch_items_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert_eq!(docs[0].as_ref().unwrap().state, ItemState::Closed);
        assert_eq!(kinds(dir.path()), ["state", "full"]);
    }

    #[test]
    fn a_failed_state_recheck_refetches_the_cached_issue_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Open);
        let rate_limited = serde_json::json!({
            "data": {"r0": null},
            "errors": [{"type": "RATE_LIMITED", "message": "slow down"}]
        })
        .to_string();
        let (bin, _shim) = state_gh(dir.path(), &rate_limited, &issue_reply("CLOSED"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let docs =
            retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert_eq!(
            (docs[0].title.as_str(), docs[0].state),
            ("fresh", ItemState::Closed)
        );
        assert_eq!(kinds(dir.path()), ["state", "full"]);
    }

    #[test]
    fn a_state_already_verified_this_run_is_not_rechecked_again() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_holding(cache_dir.path(), ItemState::Open);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("OPEN"), &issue_reply("OPEN"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        // The same item as a routed issue, then as a citation.
        retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &cache, &refs, None)).unwrap();
        let items =
            retry_on_etxtbsy(|| fetch_items_cached_current(&bin, &cache, &refs, None)).unwrap();
        assert_eq!(items[0].as_ref().unwrap().title, "cached");
        assert_eq!(kinds(dir.path()), ["state"]);
    }

    #[test]
    fn a_cached_pull_request_is_not_rechecked_when_fetched_as_an_issue() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let mut pr = cached_doc("rust-works/omni-dev", 1);
        pr.kind = ItemKind::ChangeRequest;
        cache.store(&pr);
        let later = IssueCache::new(cache_dir.path().to_path_buf(), DEFAULT_CACHE_TTL, false);
        let (bin, _shim) = state_gh(dir.path(), &state_reply("OPEN"), &issue_reply("OPEN"));
        let refs = [item_ref("rust-works/omni-dev", 1)];
        retry_on_etxtbsy(|| fetch_issues_cached_current(&bin, &later, &refs, None)).unwrap();
        assert_eq!(kinds(dir.path()), ["full"]);
    }

    /// Runs the shim once so its first `execve` (which can hit `ETXTBSY`, and
    /// which `auth_scope` would swallow into `None`) is behind us.
    ///
    /// The result is discarded *outside* the retry: `retry_on_etxtbsy` hands a
    /// non-`ETXTBSY` failure (a shim that exits 1 or prints nothing) straight
    /// back, but it can only retry an `ETXTBSY` it actually sees, so the error
    /// must not be swallowed inside the closure.
    fn warm(bin: &Path) {
        let _ = retry_on_etxtbsy(|| checked_auth_scope(bin));
    }

    #[test]
    fn auth_scope_digests_the_token_gh_reports_and_never_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "gho_secret_token", 0);
        let scope = retry_on_etxtbsy(|| checked_auth_scope(&bin)).unwrap();
        assert_eq!(scope, cache::account_scope("gho_secret_token"));
        assert!(!scope.contains("secret"), "{scope}");
        assert_eq!(auth_scope(&bin), Some(scope));
    }

    #[test]
    fn auth_scope_is_none_when_gh_cannot_say_who_it_is() {
        let dir = tempfile::tempdir().unwrap();
        assert!(auth_scope(Path::new("/no/such/gh/xyzzy")).is_none());
        let (failing, _shim) = fake_gh(dir.path(), "not logged in", 1);
        warm(&failing);
        assert!(auth_scope(&failing).is_none());
    }

    #[test]
    fn auth_scope_is_none_when_gh_prints_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let (silent, _shim) = fake_gh(dir.path(), "", 0);
        warm(&silent);
        assert!(auth_scope(&silent).is_none());
    }

    #[test]
    fn open_cache_is_namespaced_to_the_login_and_sweeps_expired_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let env = crate::test_support::env::MapEnv::new();
        let (bin, _shim) = fake_gh(dir.path(), "token-a", 0);
        warm(&bin);
        let open = || open_cache(&env, Some(base.path().to_path_buf()), &bin, false);

        open().store(&cached_doc("o/r", 1));
        let entry = base
            .path()
            .join("omni-dev/github-issues")
            .join(cache::account_scope("token-a"))
            .join("o/r/1.json");
        assert!(entry.exists(), "{entry:?}");

        // A later run, past the TTL, sweeps it on open.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&entry)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let _later = open();
        assert!(!entry.exists());
    }

    #[test]
    fn open_cache_is_off_without_a_gh_login() {
        let base = tempfile::tempdir().unwrap();
        let env = crate::test_support::env::MapEnv::new();
        let cache = open_cache(
            &env,
            Some(base.path().to_path_buf()),
            Path::new("/no/such/gh/xyzzy"),
            false,
        );
        cache.store(&cached_doc("o/r", 1));
        assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 0);
    }

    #[test]
    fn open_cache_blocking_opens_the_same_cache_from_async() {
        let dir = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "token-a", 0);
        warm(&bin);
        let cache = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(open_cache_blocking(
                crate::test_support::env::MapEnv::new(),
                Some(base.path().to_path_buf()),
                bin,
                false,
            ))
            .unwrap();
        cache.store(&cached_doc("o/r", 1));
        assert!(base
            .path()
            .join("omni-dev/github-issues")
            .join(cache::account_scope("token-a"))
            .join("o/r/1.json")
            .exists());
    }

    #[test]
    fn a_disabled_cache_always_calls_gh() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = counting_gh(dir.path(), &one_issue_reply("t"), 0);
        let refs = [item_ref("rust-works/omni-dev", 1)];
        let cache = IssueCache::disabled();
        retry_on_etxtbsy(|| fetch_issues_cached(&bin, &cache, &refs)).unwrap();
        retry_on_etxtbsy(|| fetch_items_cached(&bin, &cache, &refs)).unwrap();
        assert_eq!(gh_calls(dir.path()), 2);
    }
}
