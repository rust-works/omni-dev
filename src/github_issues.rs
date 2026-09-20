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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tracing::warn;

use crate::provider::{Comment, GitProvider, IssueDoc, ItemKind, ItemRef, ItemState};

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
            // omni-dev: coverage ignore-line reason="validated by the citation parser; every ItemRef reaching here already has a project of the form owner/repo"
            continue;
        };
        let mut frags = Vec::new();
        for (ii, item_ref) in items.iter().enumerate() {
            frags.push(item_fragment(&format!("i{ii}"), item_ref.number));
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
    let state = match node.get("state").and_then(Value::as_str) {
        Some("OPEN") => ItemState::Open,
        Some("CLOSED" | "MERGED") => ItemState::Closed,
        other => bail!("item {item_ref}: unrecognised state {other:?}"),
    };
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

/// Reads every aliased item out of an [`item_fragment`] query reply.
///
/// Unlike [`parse_issue_response`], a per-item `NOT_FOUND` is **tolerated**
/// as `None` rather than failing the whole batch: a decision comment's
/// citation can be stale or point at a typo, and one bad citation must not
/// stop `verify-decision` from checking the rest. Any other GraphQL error
/// (a missing repository, a rate limit) still fails the batch — retrying it
/// per item would not help.
fn parse_item_response(
    body: &Value,
    index: &QueryIndex,
) -> Result<HashMap<(String, u64), Option<IssueDoc>>> {
    let mut not_found: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
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
                _ => bail!(
                    "{}",
                    describe_graphql_errors(std::slice::from_ref(error), index)
                ),
            }
        }
    }

    let data = body
        .get("data")
        .context("gh api graphql response had no `data`")?;

    let mut docs = HashMap::with_capacity(index.len());
    for ((ri, ii), item_ref) in index {
        let key = (item_ref.project.clone(), item_ref.number);
        if not_found.contains(&(*ri, *ii)) {
            docs.insert(key, None);
            continue;
        }
        let node = data
            .get(format!("r{ri}"))
            .and_then(|r| r.get(format!("i{ii}")))
            .filter(|v| !v.is_null());
        match node {
            Some(node) => {
                docs.insert(key, Some(build_item_doc(item_ref, node)?));
            }
            None => {
                docs.insert(key, None);
            }
        }
    }
    Ok(docs)
}

/// Fetches a set of references that may be issues or pull requests.
///
/// Tolerates a per-reference "not found" (`None`) rather than failing the
/// whole call — used by `verify-decision` to resolve a decision comment's
/// citations. Returns one entry per `refs`, in the same order.
/// **Blocking** — callers must be on a blocking thread.
pub fn fetch_items(bin: &Path, refs: &[ItemRef]) -> Result<Vec<Option<IssueDoc>>> {
    let mut by_key = HashMap::with_capacity(refs.len());
    for chunk in refs.chunks(MAX_ISSUES_PER_QUERY) {
        let Some((query, index)) = build_item_query(chunk) else {
            // omni-dev: coverage ignore-line reason="chunks() never yields an empty chunk from a non-empty refs slice, and build_item_query returns None only for an empty slice"
            continue;
        };
        let body = crate::pr_status::run_gh_graphql(bin, &query)?;
        by_key.extend(parse_item_response(&body, &index)?);
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
    fn fetch_items_still_bails_on_a_missing_repository() {
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
        let err = retry_on_etxtbsy(|| fetch_items(&bin, &[pr_ref("no/such", 1)])).unwrap_err();
        assert!(err.to_string().contains("no/such"), "{err}");
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
}
