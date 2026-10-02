# Review and fixes

Reviewed the signed implementation commit `f743580d3ba064c3030692722070214c4c9c340b`
after committing, as requested by #2052's implementation workflow.

Findings fixed in the follow-up commit:

1. `capture.py` preserved a failing CLI's exit status in metadata but always
   returned success itself. It now returns that status after preserving raw
   stdout/stderr, including partial output. A regression test simulates exit 7.
2. A live capture could inherit `ROUTE_GH_REPLAY` and unintentionally replay
   stale GitHub fixtures. The subprocess environment now removes that variable;
   the failure regression also checks the resulting environment.
3. The fixture accepted arbitrary GraphQL operation shapes despite describing
   itself as read-only. It now requires the route's `query=query{...}` shape and
   rejects mutations before invoking gh. The original 42 reads still replay.
4. Input reconciliation checked comment IDs/bodies but omitted author logins,
   which the router includes in state. It now compares all three, including the
   production `ghost` fallback. The original summary remains byte-identical.

The capture helper was made importable for tests and its run duration is now
measured before archival/metadata work. The archived original run remains
unchanged: it exited 0, had no inherited replay setting, and used the captured
42 real read responses. No labels, selection, templates or predictions were
changed during review.

Validation:

- Current-source `cargo build --bin omni-dev` and `cargo fmt --all -- --check` passed.
- Four analysis regression tests and three capture/fixture regression tests passed.
- All 42 archived GitHub commands replayed byte-for-byte (stdout/stderr/status)
  with no network calls.
- Offline analysis reproduced `summary.json` byte-for-byte before and after fixes.
- Labels' declaration timestamp precedes the baseline; source/label hashes match.
- All 34 holdout title/body/comment inputs match the frozen pre-output revisions.
- Artifact SHA-256 checks and `git diff --check` passed.

The CLI/library are unchanged, so no CLI snapshot update or Rust behavior tests
were needed. No independent human-stage accuracy or downstream success was
claimed. The missing standalone source rubric and independent design/review
labels remain documented evidence limitations, not hidden substitutions.
