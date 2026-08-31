# Review 21 — AI fact-contract part 1

Reviewed `5bc5598` against `7900133` and the contract in
`docs/review/19-ai-fact-contract.md`. This was a static review. Per the request, I did
not run Cargo tests or Clippy. I did not change Rust source; this review file is my only
artifact.

**Counts:** BLOCKER 0 · MAJOR 0 · MINOR 5 · NIT 1

Nothing is above MINOR. The main tri-state change is fail-closed and preserves the old
closed-universe behavior. `Unknown` symbols, hunks, files, and verifiable edges are never
rendered as facts: nodes are rejected/dropped, flow edges reject their form, and non-flow
edges are removed. `StubFacts::default()` remains complete, `FixtureFacts` remains a
closed fixture, and live `AcceptAll` returns only `Present`. The remaining issues are one
false absence claim and several diagnostics/privacy/coverage gaps.

## BLOCKER

None.

## MAJOR

None.

## MINOR

1. **`crates/codescope/src/dispatcher.rs:1717-1724`; `crates/codescope-ai/src/validator.rs:199-207` — a file outside the change set is reported as proven nonexistent even though repository existence was not queried.**
   `SnapshotFacts.files` is populated from changed files (plus files attached to surfaced
   changed symbols). A `ChangeSet` is a complete inventory of *changed-file membership*,
   not a complete worktree/base file inventory. An unchanged real repository file that is
   not otherwise surfaced therefore receives `Lookup::Absent`, and the validator says
   `file ... does not exist`. That violates the lookup contract's distinction even though
   it still fails closed. **Fix:** return `Lookup::Unknown` for a miss until the request
   contract carries a complete repository-file query, or explicitly redefine this lookup
   as changed-context membership and emit `not in the current fact catalog/change set`
   rather than `does not exist`. Add a `SnapshotFacts` test with an unchanged/out-of-catalog
   file.

2. **`crates/codescope-ai/src/service.rs:170-175,282-295,343-351`; `crates/codescope-core/src/file.rs:31-40` — rejection status text can echo an absolute path from model JSON.**
   `FileId` intentionally does not validate absoluteness during deserialization. Validation
   then interpolates the model's file, symbol, or node id into a dropped reason. The new
   cleaner removes controls and scrubs secret shapes, but it never calls
   `redact_repo_root`, although `AiService` has the root. Thus a rejected entity such as
   `/Users/alice/repo/src/lib.rs` can reach `AiOutcome::Failed`, the bottom status line,
   and `AiStatus::Failed.reason`. **Fix:** make `rejection_summary` root-aware and redact
   before per-reason truncation; also prefer an absolute-path validation reason that does
   not echo the path. Add an end-to-end rejected-plan test using `REPO_ROOT` and assert the
   failure contains neither the root nor unsanitized model text.

3. **`crates/codescope-ai/src/service.rs:291-327` — the stated 240-scalar bound is off by one, and the omitted count can be truncated away.**
   `clean` takes `max` scalars and then appends `…`, so a truncated result has
   `max + 1` scalars. Two long reasons drive the final cleaner and can produce 241 scalars.
   The `(+N more)` suffix is appended after those reasons, so the final truncation can
   remove the suffix entirely even though omitted reasons exist. The current one-reason
   ASCII test never reaches either case. **Fix:** reserve one scalar for the ellipsis and
   reserve total budget for the delimiter and omitted-count suffix before truncating the
   selected reasons. Test two long multibyte reasons plus a third omitted reason, asserting
   `chars().count() <= 240` and that the omitted count survives.

4. **`crates/codescope-ai/src/service.rs:35-43,170-177` — the full rejected `ValidationReport` is not retained or logged.**
   The rejected branch borrows the typed report to build a string and then drops it.
   `AiOutcome::Failed` carries only that string, and there is no trace event containing the
   full report. This does not meet review 19's requirement to preserve the typed report for
   diagnostics/debug panes; only the first two sanitized reasons survive. **Fix:** at
   minimum emit the typed report to the intended local debug sink before flattening it.
   Prefer a structured rejected outcome such as `{ summary, report }` so the dispatcher can
   retain it without placing raw report fields on the status line.

5. **`crates/codescope-ai/src/validator.rs:1001-1075`; `crates/codescope-ai/src/service.rs:397-445`; `crates/codescope-ai/tests/scripted.rs:475-498` — the new contract is only partially covered.**
   The validator tests cover an `Unknown` flow edge and an `Unknown` symbol, but not a
   positive edge returned by a partial query, `Unknown` removal from a non-flow form,
   complete-miss symbol wording, or `Unknown` file/hunk handling. In fact,
   `StubFacts::incomplete()` only makes symbol/edge misses unknown, so it cannot express
   the latter branches. `SnapshotFacts` has no direct tests. The summary tests omit
   secret-shaped text, the absolute repo root, Unicode
   truncation, ESC/other controls, the total-bound path, and suffix preservation. The
   scripted hallucination test asserts only `plan rejected`, not that the concrete cause is
   surfaced safely. **Fix:** add the tri-state matrix and diagnostics cases above, including
   an integration assertion on the final `AiOutcome::Failed` reason.

## NIT

1. **`crates/codescope-ai/src/validator.rs:22`; `crates/codescope-ai/src/service.rs:267` — refactor comments are stale.**
   The module docs still link to removed `FactView::edge_exists`, and `Map a terminal
   error...` is now attached to `rejection_summary` rather than `outcome_from_error`.
   **Fix:** link `FactView::edge` and move the terminal-error comment back above
   `outcome_from_error`.

## Verified behavior

- `SnapshotFacts::symbol` turns every non-surfaced symbol into `Unknown`; it does not claim
  a changed-only cache is a complete outline. `SnapshotFacts::edge` likewise returns
  `Unknown` for its unqueried lazy graph. A known hunk is `Present`, and an out-of-range
  index for an enumerated changed file is `Absent`.
- Every validator branch that receives `Lookup::Unknown` fails closed. Node lookup returns
  `not queried (cannot validate)`. A flow edge rejects its form with that wording. A
  non-flow edge is recorded as dropped and is not pushed back into the form. A complete
  `Absent` edge retains `not in the impact graph`; a complete absent symbol uses
  `not found ... (analyzed)`.
- The closed test universes retain their prior behavior: `StubFacts::default().complete`
  is `true`; `FixtureFacts` maps fixture hits to `Present` and all closed-universe misses
  to `Absent`; live `AcceptAll` is all-`Present`.
- `rejection_summary` otherwise prefers `dropped` over `notes`, takes at most two reason
  entries, collapses whitespace, removes control scalars, calls `scrub_secrets`, and uses
  `chars()` rather than byte slicing. It contains no `unwrap` or model-controlled string
  slice. The dispatcher still appends `AI_FAILURE_SUFFIX` once at
  `crates/codescope/src/dispatcher.rs:1031-1038`; the service does not duplicate that
  suffix.
- Review 19's already documented no-lookup exceptions (presentational endpoints,
  `reads`/`writes`, and implied tree-child relations) remain part-2 work. They are not a
  path that converts an actual `Lookup::Unknown` into retained evidence, but capability
  honesty is still needed to remove those older bypasses.
