#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Enforces STYLE-0007's scope-list contract between the two `.omni-dev/` files.
//!
//! `scopes.yaml` is the single source of truth for commit scopes; the `## Scopes`
//! list in `commit-guidelines.md` exists only so the AI prompt has inline context.
//! Both files are injected into the same commit-check prompt — `commit-guidelines.md`
//! verbatim, `scopes.yaml` via `load_project_scopes` — so a divergence hands the AI
//! judge two documents that contradict each other. That list had drifted by nine
//! entries before anything caught it (#1421); these tests are what makes the rule
//! self-enforcing rather than aspirational.
//!
//! The same file is also the source of truth for the commit *syntax* the code has to
//! parse, so `example_subjects_are_parseable_by_parse_subject` closes the second half
//! of that contract: every subject the guidelines demonstrate must be parseable by the
//! parser every scope-aware code path runs on. The old `SCOPE_RE` had drifted the
//! other way — matching a breaking-change form the guidelines never mention while
//! failing on the one they mandate (#1473, fixed as part of #1474's `lint::parse_subject`
//! unification).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use regex::Regex;
use serde::Deserialize;

use omni_dev::data::context::ScopeDefinition;
use omni_dev::git::parse_subject;

/// Mirror of the private `ScopesConfig` in `src/claude/context/discovery.rs`.
///
/// Reuses the public [`ScopeDefinition`] so the two cannot disagree about the
/// per-scope schema; only the one-field wrapper is duplicated, which is cheaper
/// than widening the crate's public API for a test.
#[derive(Deserialize)]
struct ScopesConfig {
    scopes: Vec<ScopeDefinition>,
}

fn omni_dev_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".omni-dev")
}

/// The canonical scope set: name → description, parsed from `scopes.yaml`.
fn yaml_scopes() -> BTreeMap<String, String> {
    let path = omni_dev_dir().join("scopes.yaml");
    let raw =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let config: ScopesConfig = serde_yaml::from_str(&raw)
        .unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()));
    config
        .scopes
        .into_iter()
        .map(|scope| (scope.name, scope.description))
        .collect()
}

fn commit_guidelines() -> String {
    let path = omni_dev_dir().join("commit-guidelines.md");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Returns the body of the `## <heading>` section, up to the next `## ` heading.
///
/// `### ` subheadings do not terminate the slice, so `## Examples` keeps its
/// per-example subsections.
fn section<'a>(markdown: &'a str, heading: &str) -> &'a str {
    let marker = format!("\n## {heading}\n");
    let start = markdown
        .find(&marker)
        .unwrap_or_else(|| panic!("commit-guidelines.md has no `## {heading}` section"))
        + marker.len();
    let end = markdown[start..]
        .find("\n## ")
        .map_or(markdown.len(), |offset| start + offset);
    &markdown[start..end]
}

/// Parses `- \`name\` - description` bullets into name → description.
///
/// Prose paragraphs in the same section are ignored, which is what lets the
/// ecosystem-defaults note sit below the list without breaking parity.
fn markdown_scopes(body: &str) -> BTreeMap<String, String> {
    let bullet = Regex::new(r"^- `([a-z0-9-]+)` - (.+)$").unwrap();
    body.lines()
        .filter_map(|line| bullet.captures(line.trim_end()))
        .map(|caps| (caps[1].to_string(), caps[2].trim().to_string()))
        .collect()
}

/// STYLE-0007 clauses 1 and 3: the markdown list must match the YAML exactly.
///
/// Compared as maps rather than sequences, so `commit-guidelines.md` may stay
/// alphabetical while `scopes.yaml` keeps its curated definition order.
#[test]
fn scope_list_matches_scopes_yaml() {
    let yaml = yaml_scopes();
    let guidelines = commit_guidelines();
    let markdown = markdown_scopes(section(&guidelines, "Scopes"));

    let mut problems = Vec::new();
    for (name, description) in &yaml {
        match markdown.get(name) {
            None => problems.push(format!(
                "missing from commit-guidelines.md: `{name}` - {description}"
            )),
            Some(listed) if listed != description => problems.push(format!(
                "description differs for `{name}`:\n\
                 \x20   scopes.yaml:          {description}\n\
                 \x20   commit-guidelines.md: {listed}"
            )),
            Some(_) => {}
        }
    }
    for name in markdown.keys() {
        if !yaml.contains_key(name) {
            problems.push(format!(
                "listed in commit-guidelines.md but not defined in scopes.yaml: `{name}`"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "the `## Scopes` list in .omni-dev/commit-guidelines.md has drifted from \
         .omni-dev/scopes.yaml (STYLE-0007):\n  {}\n\n\
         scopes.yaml is the single source of truth — update the markdown list to match it.",
        problems.join("\n  ")
    );
}

/// STYLE-0007 clause 2: every scope used in `## Examples` must exist in the YAML.
///
/// Checked against `scopes.yaml` alone, not the ecosystem defaults — the examples
/// are meant to demonstrate this project's own declared scopes.
#[test]
fn example_scopes_are_defined_in_scopes_yaml() {
    let yaml = yaml_scopes();
    let guidelines = commit_guidelines();
    let examples = section(&guidelines, "Examples");

    let header = Regex::new(r"(?m)^([a-z]+)\(([^)]+)\)!?:").unwrap();
    let mut undefined = Vec::new();
    for caps in header.captures_iter(examples) {
        for scope in caps[2].split(',') {
            let scope = scope.trim();
            if !yaml.contains_key(scope) {
                undefined.push(format!("`{scope}` used in `{}`", caps[0].trim()));
            }
        }
    }

    assert!(
        undefined.is_empty(),
        "the `## Examples` section of .omni-dev/commit-guidelines.md uses scopes that \
         are not defined in .omni-dev/scopes.yaml (STYLE-0007):\n  {}",
        undefined.join("\n  ")
    );
}

/// Returns the first line of each fenced code block in `body`.
///
/// Every example in `## Examples` is a fenced commit message, so its first line is
/// the subject — precisely the input the subject parser has to handle.
fn example_subjects(body: &str) -> Vec<&str> {
    let mut subjects = Vec::new();
    let mut in_block = false;
    let mut expecting_subject = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_block = !in_block;
            expecting_subject = in_block;
        } else if expecting_subject {
            subjects.push(line);
            expecting_subject = false;
        }
    }
    subjects
}

/// Every subject form the guidelines demonstrate must be parseable, with a scope, by
/// the parser.
///
/// Closes the class rather than the instance: the old `SCOPE_RE` matched
/// `type!(scope):` — a form the guidelines never mention — and failed on the
/// `type(scope)!:` form they mandate, so the pre-validation checks and scope
/// refinement silently no-opped on every breaking-change commit (#1473). This test
/// fails the moment the guidelines teach a scoped subject the code cannot read.
#[test]
fn example_subjects_are_parseable_by_parse_subject() {
    let guidelines = commit_guidelines();
    let subjects = example_subjects(section(&guidelines, "Examples"));

    assert!(
        !subjects.is_empty(),
        "no fenced examples found in `## Examples` — the block scraper is broken, \
         which would let this test pass vacuously"
    );
    assert!(
        subjects.iter().any(|subject| subject.contains(")!:")),
        "no `type(scope)!:` breaking-change example found in `## Examples` — that form \
         is the reason this test exists (#1473), so its absence must fail loudly"
    );

    let unparseable: Vec<&str> = subjects
        .iter()
        .copied()
        .filter(|subject| parse_subject(subject).and_then(|p| p.scope).is_none())
        .collect();

    assert!(
        unparseable.is_empty(),
        "`omni_dev::git::parse_subject` cannot parse a scope out of these subjects \
         demonstrated in the `## Examples` section of .omni-dev/commit-guidelines.md:\n  {}\n\n\
         the guidelines are the source of truth — the parser must handle every scoped \
         form they teach.",
        unparseable.join("\n  ")
    );
}

#[test]
fn example_subjects_takes_the_first_line_of_each_fenced_block() {
    let body = "\n### Simple\n```\nfix(git): a subject\n```\n\n\
                ### With body\n```\nfeat(cli)!: another subject\n\n\
                BREAKING CHANGE: not a subject\n```\n";
    assert_eq!(
        example_subjects(body),
        vec!["fix(git): a subject", "feat(cli)!: another subject"]
    );
}

#[test]
fn section_stops_at_the_next_heading_but_not_subheadings() {
    let markdown = "# Title\n\n## One\n\nbody\n\n### Sub\n\nmore\n\n## Two\n\nother\n";
    assert_eq!(section(markdown, "One"), "\nbody\n\n### Sub\n\nmore\n");
    assert_eq!(section(markdown, "Two"), "\nother\n");
}

#[test]
fn markdown_scopes_ignores_prose() {
    let body = "\n- `cli` - Command-line interface and argument parsing\n\
                \nIn addition to the scopes above, `cargo` is also accepted.\n";
    let parsed = markdown_scopes(body);
    assert_eq!(parsed.len(), 1);
    assert_eq!(
        parsed.get("cli").map(String::as_str),
        Some("Command-line interface and argument parsing")
    );
}
