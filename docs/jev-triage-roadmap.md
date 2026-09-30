# Code-informed Jev triage roadmap

[#1823](https://github.com/rust-works/omni-dev/issues/1823) tracks the plan to
check what a codebase already settles before assigning an issue to a model
class. The work is split into independently reviewable issues below. This page
records the delivery order; it does not describe shipped CLI behavior.

## Delivery order

1. Preview the exact decision comment before posting it:
   [#1821](https://github.com/rust-works/omni-dev/issues/1821).
2. Surface close calls by tier margin:
   [#2050](https://github.com/rust-works/omni-dev/issues/2050), coordinated
   with the runner-up display in
   [#1871](https://github.com/rust-works/omni-dev/issues/1871).
3. Report the stage that supplied `class`:
   [#2051](https://github.com/rust-works/omni-dev/issues/2051).
4. Establish a held-out routing baseline before adding new questions:
   [#2052](https://github.com/rust-works/omni-dev/issues/2052).
5. Classify open questions as factual, design, both, or none:
   [#2053](https://github.com/rust-works/omni-dev/issues/2053).
6. Check for existing definitions in one bounded round:
   [#2054](https://github.com/rust-works/omni-dev/issues/2054).
7. Test whether cited, templated findings move the design stage when previewed:
   [#2055](https://github.com/rust-works/omni-dev/issues/2055).
8. Verify symbol and commit citations before a finding can be posted:
   [#2056](https://github.com/rust-works/omni-dev/issues/2056).
9. Compose the validated pieces into one-pass triage:
   [#2057](https://github.com/rust-works/omni-dev/issues/2057).
10. Study a per-call-site Jev audit:
    [#2058](https://github.com/rust-works/omni-dev/issues/2058).

The code-only reporting changes can land independently. New Jev questions and
retrieval behavior depend on the baseline in #2052. The orchestrator depends
on previewing and verifying the exact comment it would post; it performs at
most one model search. Each feature issue owns its implementation tests,
user-facing documentation, and changelog entry when it ships.

The existing route stage questions and tier descriptions remain unchanged.
Code and referenced issue content stay out of `route` state. Jev classifies
bounded inputs; code retrieves, counts, and executes. A factual signal alone
does not change the stage tier. Before any automated comment, a human or an
explicit gate must check each `Settled` claim against its cited source;
checking that the citation merely exists is insufficient.
