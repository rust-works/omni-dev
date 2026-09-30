//! Shared citation-finding for the `jev` commands (#1812).
//!
//! Recognises an issue/pull-request reference in free text — `#N`, `PR #N` /
//! `pull request #N` (case-insensitive), `owner/repo#N`, and a full GitHub
//! issue/pull URL — and resolves it to an [`ItemRef`]. Originally written for
//! `verify-decision` (#1779) and factored out here so `route`'s `depends_on`
//! (#1812) can reuse it rather than duplicate the regex and its boundary
//! rules, which is exactly the kind of "silently diverging" duplication this
//! project's own optimization notes call out.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::{Captures, Regex};

use crate::jev::markdown_code::mask_code;
use crate::provider::{ItemKind, ItemRef};

/// A reference to another issue or pull request found in a comment's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// What the reference resolves to.
    pub item_ref: ItemRef,
    /// The exact text matched, e.g. `"#1614"` or `"PR #1629"` — reused
    /// verbatim in the splitter prompt, since the splitter is asked to
    /// write `cites` "as it appears in the comment".
    pub raw: String,
    /// Resolved web URL, populated by route's item lookup when available.
    pub url: Option<String>,
}

fn citation_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    #[allow(clippy::expect_used)]
    RE.get_or_init(|| {
        Regex::new(
            r"(?ix)
              https://github\.com/(?P<url_owner>[A-Za-z0-9_.-]+)/(?P<url_repo>[A-Za-z0-9_.-]+)/(?P<url_kind>issues|pull)/(?P<url_num>[0-9]+)
            | (?P<other_url>https?://\S+)
            | (?P<or_owner>[A-Za-z0-9_.-]+)/(?P<or_repo>[A-Za-z0-9_.-]+)\#(?P<or_num>[0-9]+)
            | (?:pr|pull\ request)\s*\#(?P<pr_num>[0-9]+)
            | \#(?P<issue_num>[0-9]+)
            ",
        )
        .expect("citation regex must compile")
    })
}

/// The byte offset just past the matched number, whichever alternative
/// matched, or `None` for the `other_url` alternative (which matches no
/// number by design).
fn number_end(caps: &Captures<'_>) -> Option<usize> {
    ["url_num", "or_num", "pr_num", "issue_num"]
        .iter()
        .find_map(|name| caps.name(name))
        .map(|m| m.end())
}

/// Whether the character following a match ending at byte `end` really ends
/// the reference.
///
/// The number must not run straight into a word character. Without this a
/// hex colour (`#1f77b4`) or a heading anchor (`#1-overview`) parses as
/// issue `#1` in the current repository — which resolves to a real,
/// unrelated issue and becomes a source — and a doc link
/// (`docs/jev.md#4-state-input`) invents the repository `docs/jev.md`,
/// whose repository-level `NOT_FOUND` fails the whole run.
fn ends_at_boundary(body: &str, end: usize) -> bool {
    body[end..]
        .chars()
        .next()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// Reads one regex match into `(project, kind hint, number)`. The kind is
/// only a hint — [`crate::github_issues::fetch_items`] resolves the real
/// kind from GitHub's `__typename`, since a comment can call a pull
/// request "#1629" or an issue "PR #1614" and only the API knows which it is.
fn citation_from_captures(
    caps: &Captures<'_>,
    default_project: &str,
) -> Option<(String, ItemKind, u64)> {
    if let Some(n) = caps.name("url_num") {
        let kind = if &caps["url_kind"] == "pull" {
            ItemKind::ChangeRequest
        } else {
            ItemKind::Issue
        };
        n.as_str().parse::<u64>().ok().map(|number| {
            (
                format!("{}/{}", &caps["url_owner"], &caps["url_repo"]),
                kind,
                number,
            )
        })
    } else if let Some(n) = caps.name("or_num") {
        n.as_str().parse::<u64>().ok().map(|number| {
            (
                format!("{}/{}", &caps["or_owner"], &caps["or_repo"]),
                ItemKind::Issue,
                number,
            )
        })
    } else if let Some(n) = caps.name("pr_num") {
        n.as_str()
            .parse::<u64>()
            .ok()
            .map(|number| (default_project.to_string(), ItemKind::ChangeRequest, number))
    } else if let Some(n) = caps.name("issue_num") {
        n.as_str()
            .parse::<u64>()
            .ok()
            .map(|number| (default_project.to_string(), ItemKind::Issue, number))
    } else {
        // omni-dev: coverage ignore-line reason="unreachable: both call sites (find_citations, first_citation) already filtered out the only alternative with no numbered group (other_url) via number_end before calling this"
        None
    }
}

/// Reads the first citation in `text`, skipping a match that does not end
/// at a boundary (see [`ends_at_boundary`]) or that is a non-issue URL.
pub(crate) fn first_citation(text: &str, default_project: &str) -> Option<(String, ItemKind, u64)> {
    citation_regex().captures_iter(text).find_map(|caps| {
        let end = number_end(&caps)?;
        if !ends_at_boundary(text, end) {
            return None;
        }
        citation_from_captures(&caps, default_project)
    })
}

/// Finds every issue/pull-request reference in `body`.
///
/// Recognises `#N`, `PR #N` / `pull request #N` (case-insensitive),
/// `owner/repo#N`, and full GitHub issue/pull URLs. Drops a citation of
/// the judged issue itself, and deduplicates by `(project, number)`,
/// keeping the first occurrence's raw text.
///
/// Scans **all** of `body`, code included: `verify-decision` checks each
/// citation against its source, so a reference deliberately written in
/// backticks (`` `#1614` ``) must still be checked, and a missed source
/// weakens a verifier where an extra one only costs a lookup. `route` skips
/// code through [`find_citations_outside_code`] instead (#2003).
#[must_use]
pub fn find_citations(body: &str, default_project: &str, judged: &ItemRef) -> Vec<Citation> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for caps in citation_regex().captures_iter(body) {
        let Some(end) = number_end(&caps) else {
            continue; // a non-issue URL, matched only so it cannot be mis-read
        };
        if !ends_at_boundary(body, end) {
            continue;
        }
        let Some((project, kind, number)) = citation_from_captures(&caps, default_project) else {
            continue;
        };
        if project == judged.project && number == judged.number {
            continue;
        }
        if seen.insert((project.clone(), number)) {
            out.push(Citation {
                item_ref: ItemRef {
                    provider: judged.provider,
                    project,
                    kind,
                    number,
                },
                raw: caps[0].to_string(),
                url: None,
            });
        }
    }
    out
}

/// Finds every issue/pull-request reference in `body` that is not inside a
/// fenced code block or an inline code span (#2003).
///
/// Otherwise identical to [`find_citations`]. `route`'s `depends_on` uses
/// this: a quoted error message or example such as `owner/repo#123` is not a
/// dependency, and treating it as one costs a pointless `gh` lookup and adds
/// noise to `reference_fetch_failures`. See [`mask_code`] for exactly what
/// counts as code.
#[must_use]
pub fn find_citations_outside_code(
    body: &str,
    default_project: &str,
    judged: &ItemRef,
) -> Vec<Citation> {
    find_citations(&mask_code(body), default_project, judged)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::provider::GitProvider;

    fn judged(number: u64) -> ItemRef {
        ItemRef {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            kind: ItemKind::Issue,
            number,
        }
    }

    #[test]
    fn finds_bare_hash_and_owner_repo_and_url_forms() {
        let body = "See #1614, other/repo#5, and https://github.com/rust-works/omni-dev/pull/1629.";
        let cites = find_citations(body, "rust-works/omni-dev", &judged(1779));
        assert_eq!(cites.len(), 3);
        assert_eq!(cites[0].item_ref.number, 1614);
        assert_eq!(cites[0].item_ref.kind, ItemKind::Issue);
        assert_eq!(cites[1].item_ref.project, "other/repo");
        assert_eq!(cites[2].item_ref.number, 1629);
        assert_eq!(cites[2].item_ref.kind, ItemKind::ChangeRequest);
    }

    #[test]
    fn a_github_issue_url_resolves_to_issue_kind() {
        let cites = find_citations(
            "see https://github.com/rust-works/omni-dev/issues/1614",
            "rust-works/omni-dev",
            &judged(1779),
        );
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].item_ref.kind, ItemKind::Issue);
        assert_eq!(cites[0].item_ref.number, 1614);
    }

    /// A citation number too large for `u64` fails to parse; the citation is
    /// skipped rather than propagating the parse failure.
    #[test]
    fn an_overflowing_citation_number_is_skipped() {
        let cites = find_citations(
            "see #99999999999999999999 and #1614",
            "rust-works/omni-dev",
            &judged(1779),
        );
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].item_ref.number, 1614);
    }

    #[test]
    fn pr_prefix_wins_over_the_bare_hash_alternative() {
        let cites = find_citations("Fixed by PR #1629.", "rust-works/omni-dev", &judged(1));
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].item_ref.kind, ItemKind::ChangeRequest);
        assert_eq!(cites[0].raw, "PR #1629");
    }

    #[test]
    fn pull_request_prefix_is_case_insensitive() {
        let cites = find_citations("see pull request #42 for context", "o/r", &judged(1));
        assert_eq!(cites[0].item_ref.number, 42);
        assert_eq!(cites[0].item_ref.kind, ItemKind::ChangeRequest);
    }

    #[test]
    fn drops_self_citations_and_dedupes() {
        let cites = find_citations(
            "#1779 depends on #1614, see also #1614 again",
            "rust-works/omni-dev",
            &judged(1779),
        );
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].item_ref.number, 1614);
    }

    #[test]
    fn returns_nothing_for_a_comment_with_no_citations() {
        assert!(find_citations("Looks good to me!", "o/r", &judged(1)).is_empty());
    }

    /// A number that runs into a word character is not a citation. Without
    /// the boundary check a hex colour or a heading anchor resolved to a
    /// real, unrelated issue and became a source Jev was asked about.
    #[test]
    fn a_number_running_into_a_word_character_is_not_a_citation() {
        for body in [
            "the colour #1f77b4 is used",
            "see the anchor #1-overview below",
        ] {
            assert!(
                find_citations(body, "rust-works/omni-dev", &judged(1779)).is_empty(),
                "{body}"
            );
        }
    }

    /// A relative doc link is not `owner/repo#N`. Without this the invented
    /// project `docs/jev.md` reached `gh`, whose repository-level
    /// `NOT_FOUND` fails the whole run rather than one citation.
    #[test]
    fn a_doc_anchor_link_does_not_invent_a_repository() {
        let cites = find_citations(
            "see [state input](docs/jev.md#4-state-input) and #1614",
            "rust-works/omni-dev",
            &judged(1779),
        );
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].item_ref.project, "rust-works/omni-dev");
        assert_eq!(cites[0].item_ref.number, 1614);
    }

    /// A URL that is not a GitHub issue/pull link is swallowed whole, so
    /// neither its path nor its fragment can be read as a citation.
    #[test]
    fn a_non_issue_url_yields_no_citation() {
        for body in [
            "see https://github.com/rust-works/omni-dev/blob/main/README.md#12",
            "see https://example.com/browse/X#1614",
        ] {
            assert!(
                find_citations(body, "rust-works/omni-dev", &judged(1779)).is_empty(),
                "{body}"
            );
        }
    }

    #[test]
    fn a_citation_followed_by_punctuation_is_still_found() {
        for body in ["fixed by #1614.", "fixed by #1614", "fixed by (#1614)"] {
            let cites = find_citations(body, "rust-works/omni-dev", &judged(1779));
            assert_eq!(cites.len(), 1, "{body}");
            assert_eq!(cites[0].raw, "#1614", "{body}");
        }
    }

    const CODE_BODY: &str = "\
Real one: #1614.

```
error: could not find owner/repo#123
```

Inline `o/r#1` and ``o/r#2`` examples, then PR #1629.
";

    /// `route` ignores a quoted example: neither the fenced block nor the
    /// inline spans yield a citation, and the ones outside code still do.
    #[test]
    fn outside_code_skips_fences_and_inline_spans() {
        let cites = find_citations_outside_code(CODE_BODY, "rust-works/omni-dev", &judged(1779));
        let numbers: Vec<u64> = cites.iter().map(|c| c.item_ref.number).collect();
        assert_eq!(numbers, [1614, 1629]);
        assert_eq!(cites[0].raw, "#1614");
        assert_eq!(cites[1].raw, "PR #1629");
    }

    /// `verify-decision` keeps scanning code, so a reference deliberately
    /// written in backticks is still checked against its source.
    #[test]
    fn find_citations_still_reads_code() {
        let cites = find_citations(CODE_BODY, "rust-works/omni-dev", &judged(1779));
        let numbers: Vec<u64> = cites.iter().map(|c| c.item_ref.number).collect();
        assert_eq!(numbers, [1614, 123, 1, 2, 1629]);
    }

    #[test]
    fn a_backticked_reference_is_found_by_find_citations_but_not_outside_code() {
        let body = "settled by `#1614`";
        let judged = judged(1779);
        assert_eq!(
            find_citations(body, "rust-works/omni-dev", &judged).len(),
            1
        );
        assert!(find_citations_outside_code(body, "rust-works/omni-dev", &judged).is_empty());
    }

    #[test]
    fn outside_code_matches_find_citations_when_there_is_no_code() {
        let body = "See #1614, other/repo#5, and https://github.com/rust-works/omni-dev/pull/1629.";
        let judged = judged(1779);
        assert_eq!(
            find_citations_outside_code(body, "rust-works/omni-dev", &judged),
            find_citations(body, "rust-works/omni-dev", &judged)
        );
    }

    /// A citation ending right where a span begins is still a citation: the
    /// mask character is not a word character.
    #[test]
    fn outside_code_keeps_a_citation_that_abuts_a_span() {
        let cites =
            find_citations_outside_code("fixed by #1614`x`", "rust-works/omni-dev", &judged(1779));
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].raw, "#1614");
    }

    /// An unclosed fence must not hide the citations after it.
    #[test]
    fn outside_code_ignores_an_unclosed_fence() {
        let cites = find_citations_outside_code(
            "```\nunclosed\n\nlater #1614",
            "rust-works/omni-dev",
            &judged(1779),
        );
        assert_eq!(cites.len(), 1);
    }
}
