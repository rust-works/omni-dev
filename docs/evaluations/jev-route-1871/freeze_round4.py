#!/usr/bin/env python3
"""Freeze the round-4 implementation-stage inputs from the #2052 census.

Screening is offline and uses only the archived #2052 baseline (`jev-1.13.0`,
Anthropic ladder): every issue whose routed `implement` stage was above the
bottom tier, plus the next four by P(top tier). Text comes from the archived
REST responses in `jev-route-2052/sources.json.gz`, so it is exactly the text
that baseline saw. Anything not archived (#2839, which has no triage comment and
so is not among the 34 archived census issues) is fetched live with `gh api`.
Its `updated_at` is recorded and must precede the census collection, so the
text was not edited after that baseline ran; this is a check on the timestamp,
not a recovery of the exact revision the census saw.

The spike candidates are chosen by a recorded *keyword* scan of the census plus
the open omni-dev research/experiment issues -- never by looking at Jev scores.

Usage: freeze_round4.py IMPLEMENT_OUTPUT.json SPIKE_OUTPUT.json
No credentials are read; the output holds public issue text only.
"""
import gzip
import json
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
CENSUS = HERE.parent / "jev-route-2052"
REPO = "rust-works/succinctly"
EXTRA_NEARBY = 4
# Keyword hits (measure/benchmark/probe/experiment/spike/"if ... then") over the
# 34 archived census texts, plus open omni-dev research/experiment issues.
SPIKE_CENSUS = [2607, 2663, 2708, 2784, 2806, 3241, 3455, 3460, 3479]
SPIKE_OMNI_DEV = [1816, 2054, 2055, 2058, 2122, 2132, 2136]
# Added after the first scan found a single clear positive: open succinctly
# issues matching `gh search issues --repo rust-works/succinctly --state open
# "re-measure"` (also tried: "measurement gate", "decide from the numbers",
# "decision threshold"; only #2663 matched those and is already in the census set).
SPIKE_SUCCINCTLY_SEARCH = [1993, 3685, 3698]


def load_census():
    baseline = json.load(gzip.open(CENSUS / "baseline.json.gz"))
    sources = {s["number"]: s for s in json.load(gzip.open(CENSUS / "sources.json.gz"))}
    return baseline["issues"], sources


def screen(issues):
    """Return (above_floor, nearest) refs, ranked by P(top tier) descending."""
    rows = []
    for issue in issues:
        stage = issue["providers"]["anthropic"]["stages"]["implement"]
        top = max(stage["probabilities"], key=lambda k: (k != "sonnet", k))
        rows.append((stage["probabilities"].get(top, 0.0), stage["choice"], issue))
    rows.sort(key=lambda r: (-r[0], r[2]["ref"]))
    above = [r for r in rows if r[1] != "sonnet"]
    nearest = [r for r in rows if r[1] == "sonnet"][:EXTRA_NEARBY]
    return above, nearest


def comments_of(pages):
    out = []
    for page in pages:
        for c in page:
            user = c.get("user") or {}  # a deleted account has no user
            if user.get("type") == "Bot":
                continue
            out.append({"author": user.get("login", "ghost"), "body": c["body"], "id": c["id"]})
    return out


def from_archive(source):
    issue = source["issue"]
    return {
        "updated_at": issue["updated_at"],
        "state": issue["state"],
        "title": issue["title"],
        "body": issue["body"] or "",
        "comments": comments_of(source["comment_pages"]),
        "origin": "jev-route-2052/sources.json.gz",
    }


def census_cutoff():
    """Earliest time the census collected any archived source."""
    return min(s["fetched_at"] for s in json.load(gzip.open(CENSUS / "sources.json.gz")))


def from_gh(number, repo=REPO, must_predate_census=False):
    def api(path):
        return json.loads(
            subprocess.run(
                ["gh", "api", path, "--paginate", "--slurp"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout
        )

    issue = api(f"repos/{repo}/issues/{number}")[0]
    if must_predate_census and issue["updated_at"] >= census_cutoff():
        raise SystemExit(
            f"#{number} was edited at {issue['updated_at']}, after the census collection "
            f"({census_cutoff()}); its live text is not the text that baseline saw"
        )
    comments = api(f"repos/{repo}/issues/{number}/comments?per_page=100")
    return {
        "updated_at": issue["updated_at"],
        "state": issue["state"],
        "title": issue["title"],
        "body": issue["body"] or "",
        "comments": comments_of(comments),
        "origin": "gh api at freeze time",
    }


def case(ref, rank_reason, probs, text, citations, repo=REPO):
    number = int(ref.rsplit("#", 1)[1])
    short = repo.split("/")[1]
    return {
        "id": f"{short}-{number}-r4",
        "source_url": f"https://github.com/{repo}/issues/{number}",
        "source_updated_at": text["updated_at"],
        "reconstructed": False,
        "screening": {"reason": rank_reason, "baseline_implement_probabilities": probs},
        "origin": text["origin"],
        "citations": citations,
        "doc": {
            "provider": "github",
            "project": repo,
            "number": number,
            "kind": "issue",
            "title": text["title"],
            "state": text["state"],
            "body": text["body"],
            "comments": text["comments"],
            "closed_by": [],
            "url": f"https://github.com/{repo}/issues/{number}",
        },
    }


def spike_cases(issues, sources):
    by_ref = {i["ref"]: i for i in issues}
    cases = []
    for number in SPIKE_CENSUS:
        text = from_archive(sources[number])
        ref = f"{REPO}#{number}"
        probs = by_ref[ref]["providers"]["anthropic"]["stages"]["implement"]["probabilities"]
        cases.append(case(ref, "spike keyword scan: census", probs, text, []))
    for number in SPIKE_SUCCINCTLY_SEARCH:
        text = from_gh(number)
        cases.append(case(f"{REPO}#{number}", "spike keyword search: succinctly", None, text, []))
    for number in SPIKE_OMNI_DEV:
        text = from_gh(number, "rust-works/omni-dev")
        cases.append(
            case(f"rust-works/omni-dev#{number}", "spike keyword scan: omni-dev", None, text, [],
                 repo="rust-works/omni-dev")
        )
    return cases


def main():
    out = pathlib.Path(sys.argv[1])
    spike_out = pathlib.Path(sys.argv[2])
    issues, sources = load_census()
    above, nearest = screen(issues)
    cases = []
    for group, reason in ((above, "implement above bottom tier"), (nearest, "nearest to boundary")):
        for _, _, issue in group:
            number = int(issue["ref"].rsplit("#", 1)[1])
            text = (
                from_archive(sources[number])
                if number in sources
                else from_gh(number, must_predate_census=True)
            )
            citations = [d["ref"] for d in issue.get("depends_on", [])]
            probs = issue["providers"]["anthropic"]["stages"]["implement"]["probabilities"]
            cases.append(case(issue["ref"], reason, probs, text, citations))
    out.write_text(json.dumps(cases, indent=2, ensure_ascii=False) + "\n")
    spikes = spike_cases(issues, sources)
    spike_out.write_text(json.dumps(spikes, indent=2, ensure_ascii=False) + "\n")
    for c in cases + spikes:
        d = c["doc"]
        print(
            c["id"], d["state"], c["source_updated_at"], c["origin"][:12],
            f"body={len(d['body'])} comments={len(d['comments'])}", c["citations"],
        )


if __name__ == "__main__":
    main()
