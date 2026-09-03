# Review response — what was fixed

8 fresh sub-agent reviews produced docs/review/01..08. This file records the disposition.
Historical gate when this review response was written: 371 tests, 0 failures, and
`cargo clippy --workspace --all-targets -- -D warnings` clean. These are archival counts, not the
current repository gate. End-to-end at that time: fixture rendered, analysis ran, and gopls exited
0 on quit.

## Fixed (by review finding id)

LSP (02): **F1** base-overlay positions converted against the overlay text (was worktree/empty —
confirmed-live wrong columns); **F2** deleted-file restore now `didClose` instead of an empty
overlay (was phantom diagnostics); **F3** graceful `shutdown → exit → kill` is now reachable —
the dispatcher shuts the engine down at loop end and `main` awaits it with a bound instead of
`abort()`; **F6** gopls marks `PushDiagnostics` Supported.

Git (03): **G1** non-UTF-8 file content no longer fails the whole changeset (lossy decode of
hunk content, strict structural lines); **G2** `-c diff.suppressBlankEmpty=false`; **G3**
`--submodule=short`; **G4** `-c core.fsmonitor=false` (read-only: no daemon spawned);
**G5** `fingerprint()` folds in changed-file size+mtime (repeat edits now detected) + test;
**G6** extended env hardening (`GIT_DIFF_OPTS`, object/namespace/config vars); **G7**
`--no-show-signature`.

Perf/concurrency (06): **H1** the dispatcher no longer awaits git/LSP/AI inline — refresh and AI
run as spawned, epoch-tagged jobs applied only when the epoch still matches (`on_analysis_done` /
`on_ai_done`); **H2** startup is non-blocking — the TUI comes up immediately, gopls initializes in
the background and is handed over via `EngineReady`; **H3** partially (jobs are epoch-gated;
content-hash caching and call-hierarchy laziness are deferred, see below).

UI (05): **H1** scope actions now reach the dispatcher (`set_scope` re-runs the pipeline) instead
of only relabeling; **H3** superseded AI rows are epoch-checked in `panes()` and replaced by a
"stale, regenerating" note; **H4** the expand/collapse off-by-one is fixed (`<` not `<=`) with
directional expand/collapse and a per-file mapping fix; **H5** errors now surface in
`UiSnapshot.message` (analysis failure, AI not configured, AI failure) instead of silence.

Privacy (07): **P1** added the missing layer-4 content scrubber (`codescope-ai/src/scrub.rs`):
secret-shaped assignments, JWTs, bearer tokens, PEM headers, and common provider key shapes are
replaced with `[redacted-secret]` before any digest leaves the machine (5 tests).

Tests (08): **T1** terminal-restore pty test (`crates/codescope/tests/terminal_restore.rs`):
the binary panics under `CODESCOPE_TEST_PANIC` and the test asserts the leave-alternate-screen
bytes are emitted and the exit is non-zero. **T2** partially: dispatcher epoch gating is now
real and reviewable; a dedicated epoch-coalescing test is listed below.

Language neutrality (01): the `gopls:` top-bar label is now generic (`lsp:`), and a
`LanguageService::handles(FileId)` ownership seam routes files (gopls owns `*.go`); the analysis
engine skips non-owned files instead of mislabeling them as Go.

## Deferred (recorded in docs/limitations.md / FIX-PLAN)

- rv-perf H3 remainder: per-file content-hash cache, lazy per-selection call-hierarchy,
  and one-sync-per-file-per-refresh (the epoch-gated job model is the correctness half;
  these are the efficiency half).
- rv-perf M4 (blocking std::fs in the gopls adapter → tokio::fs), M5 (dedupe git spawns),
  L1 (render-on-change instead of 30 fps clone), L2 (writer task).
- rv-git lows: gone-upstream display, gitlink↔file path dups, dir-path `git show` guard,
  unknown porcelain record tags.
- rv-ui mediums/lows not listed above (spinner timing, LsStatus transitions, legend, hunk
  scroll-to, narrow-tier polish) — tracked in FIX-PLAN.
- rv-tests T3/T4 (kill-escalation + fake-lsp wired to the real client) and the AI-review
  (rv-ai never returned; at review time the AI crate had an 81-test suite incl. hallucination,
  stale-epoch, circuit-breaker, redaction cases).

The architecture doc's decision 4 now matches the implemented spawn + epoch-gate model.
