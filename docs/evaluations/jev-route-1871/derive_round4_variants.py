#!/usr/bin/env python3
"""Derive the round-4 edited variants from the frozen `round4-inputs.json`.

Two kinds of edit, both recorded on the case (`derived_from`, `edit`) so a reader
can see exactly what changed:

* `-noclass`: remove the triage comment's `**Class:** ...` paragraph. Every
  census issue that routed `implement` above the bottom tier carries one, and it
  states the class and why. That is a confound for any claim about what the
  text alone supports, so each such case is also run without it.
* `-inject-open` / `-inject-resolved`: an **edited simulation**, the weakest
  grade of evidence here. The natural text labels every citation `no`
  (independent, sibling, coordinate), so no real above-floor `yes` pair exists
  in this pool. These two cases rewrite ONE dependency clause to state an
  explicit hard prerequisite (open) or that it has landed (resolved), so the
  question and the stage answer can be probed for any joint sensitivity. They
  are labelled `yes`/resolved by construction and are never counted as natural
  evidence.

A post-hoc kind exists too (`--kind absorb`): the prerequisite injection above
turned out to test the wrong thing, because a hard prerequisite orders work but
does not shrink THIS issue's implementation, which is what the question asks.
`-absorb-open` / `-absorb-resolved` instead state that the cited issue delivers
part of this issue's implementation. They were written AFTER the first run's
scores were read, so they are exploratory and never enter a gate decision.

Usage: derive_round4_variants.py INPUTS.json OUTPUT.json [--kind prerequisite|absorb]
"""
import copy
import json
import pathlib
import re
import sys

CLASS_PARAGRAPH = re.compile(r"(?m)^\*\*Class:?\*\*.*?(?:\n\n|\Z)", re.S)

INJECTIONS = {
    "succinctly-2800-r4": {
        "citation": "#2801",
        "original": "#2801 (path register — independent, but `setpath([1]; v)` on a mapping needs this issue's int-index lookup)",
        "open": "#2801 (path register — a hard prerequisite: this issue's wildcard and integer-index traversal returns each key through the path register that #2801 introduces, so the traversal arms cannot be written until #2801 lands)",
        "resolved": "#2801 (path register — landed: this issue's wildcard and integer-index traversal now returns each key through the path register it introduced)",
    },
    "succinctly-2799-r4": {
        "citation": "#2802",
        "original": "#2802 (spellings — affects which text the key/compare sees, not the mechanism)",
        "open": "#2802 (spellings — a hard prerequisite: the key and compare text rules below cannot be written until #2802 lands, because the document's original spelling is not available to them until then)",
        "resolved": "#2802 (spellings — landed: the document's original spelling is now available to the key and compare text rules below)",
    },
}


ABSORB = {
    "succinctly-2800-r4": {
        "citation": "#2801",
        "original": INJECTIONS["succinctly-2800-r4"]["original"],
        "open": "#2801 (path register — overlapping scope: #2801 will also deliver this issue's integer-index lookup and wildcard key matcher, so most of the traversal work is done there and only the `has()` and delete sites would remain here)",
        "resolved": "#2801 (path register — landed: it also delivered this issue's integer-index lookup and wildcard key matcher, so only the `has()` and delete sites remain here)",
    },
    "succinctly-2799-r4": {
        "citation": "#2802",
        "original": INJECTIONS["succinctly-2799-r4"]["original"],
        "open": "#2802 (spellings — overlapping scope: #2802 will also deliver the key and compare text rules below, so only the first-occurrence ordering and the `sort` comparator would remain here)",
        "resolved": "#2802 (spellings — landed: it also delivered the key and compare text rules below, so only the first-occurrence ordering and the `sort` comparator remain here)",
    },
}


def replace_once(doc, old, new):
    """Replace `old` exactly once across the body and comments."""
    hits = 0
    for holder in [doc] + doc["comments"]:
        key = "body"
        count = holder[key].count(old)
        if count:
            holder[key] = holder[key].replace(old, new)
            hits += count
    assert hits == 1, f"expected exactly one occurrence, found {hits}: {old[:60]!r}"


def strip_class(doc):
    removed = []
    for holder in [doc] + doc["comments"]:
        match = CLASS_PARAGRAPH.search(holder["body"])
        if match:
            removed.append(match.group(0).strip())
            holder["body"] = (holder["body"][: match.start()] + holder["body"][match.end():]).rstrip() + "\n"
    assert len(removed) == 1, f"expected one class paragraph, found {len(removed)}"
    return removed[0]


def main():
    cases = json.load(open(sys.argv[1]))
    kind = "prerequisite"
    if "--kind" in sys.argv:
        kind = sys.argv[sys.argv.index("--kind") + 1]
        assert kind in ("prerequisite", "absorb"), kind
    out = []
    for case in cases if kind == "prerequisite" else []:
        holders = [case["doc"]] + case["doc"]["comments"]
        if not any(CLASS_PARAGRAPH.search(h["body"]) for h in holders):
            continue
        variant = copy.deepcopy(case)
        removed = strip_class(variant["doc"])
        text = json.dumps(variant["doc"], ensure_ascii=False)
        assert not re.search(r"\b(Opus|Sonnet|Fable)\b", text), f"{case['id']} still names a class"
        variant["id"] = case["id"] + "-noclass"
        variant["derived_from"] = case["id"]
        variant["edit"] = {"kind": "remove the triage class paragraph", "removed": removed}
        out.append(variant)
    by_id = {c["id"]: c for c in cases}
    for base_id, spec in (INJECTIONS if kind == "prerequisite" else ABSORB).items():
        tag = "inject" if kind == "prerequisite" else "absorb"
        base = by_id[base_id]
        others = [c for c in base["citations"] if c != spec["citation"]]
        for state in ("open", "resolved"):
            variant = copy.deepcopy(base)
            replace_once(variant["doc"], spec["original"], spec[state])
            variant["id"] = f"{base_id}-{tag}-{state}"
            variant["derived_from"] = base_id
            variant["reconstructed"] = True
            variant["edit"] = {
                "kind": f"edited simulation ({'explicit hard prerequisite' if kind == 'prerequisite' else 'cited issue delivers part of this issue'})"
                if state == "open"
                else f"edited simulation ({'the prerequisite has landed' if kind == 'prerequisite' else 'the overlapping work has landed'})",
                **({"posthoc": True} if kind == "absorb" else {}),
                "citation": spec["citation"],
                "from": spec["original"],
                "to": spec[state],
            }
            if state == "open":
                variant["citations"] = base["citations"]
            else:
                variant["citations"] = others
                variant["variants"] = ["baseline"]
            out.append(variant)
    pathlib.Path(sys.argv[2]).write_text(json.dumps(out, indent=2, ensure_ascii=False) + "\n")
    for v in out:
        print(v["id"], v["citations"], v.get("variants", ["baseline", "candidate"]))


if __name__ == "__main__":
    main()
