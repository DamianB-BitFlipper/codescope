# codescope — review fix pass (tracking)

7 of 8 reviews in. Counts: 11 high / ~30 med / ~30 low. Verdicts: lang=ship, lsp=fix-first,
git=fix-first, ui=fix-first, perf=fix-first, privacy=fix-first, tests=fix-first.

## HIGH (fix now)

LSP (rv-lsp):
- F1 base_document_symbols converts utf-16 wire positions against WRONG text (reads worktree
  instead of overlay content). -> symbol_tree must take the text as a param.
- F2 deleted-file restore reopens an EMPTY overlay instead of didClose -> phantom diagnostics.
- F3 graceful shutdown is dead code in the binary (main aborts dispatcher -> SIGKILL via
  kill_on_drop). -> dispatcher::run should shut down the engine's service on loop end.

PERF (rv-perf):
- H1 dispatcher awaits everything inline; no spawned jobs, no apply-time epoch gate. The
  epoch mechanism holds only vacuously. -> spawn analysis/AI as epoch-tagged jobs; drop stale.
- H2 startup blocks on gopls initialize (60s) before the TUI. -> TUI first, LS in background.
- H3 refresh amplification: no coalescing, no per-file cache, eager per-symbol fan-out.

UI (rv-ui):
- H1 scope keys are label-only; data never changes (dispatcher scope fixed to Branch).
- H2 selection never drives diff/semantic panes (center always first file); Enter mis-routed.
- H3 superseded AI view renders unmarked (epoch ignored in panes()).
- H4 expand/collapse off-by-one (idx <= symbols.len() should be idx < symbols.len()).
- H5 status/error message surface is dead; failed first refresh hangs on placeholder.

GIT (rv-git):
- G1 one non-UTF-8 text file fails the whole changeset (wedges refresh). -> lossy-decode
  hunk content, keep strict paths; or degrade that file.

PRIVACY (rv-privacy):
- P1 4-layer exclusion unimplemented past layer 1: tracked secrets/inline keys reach the
  provider via digest hunk previews. -> add secret content sniffing + denylist on digest.

TESTS (rv-tests):
- T1 terminal restore has zero coverage (portable-pty declared, unused). -> pty panic test.
- T2 rapid changes/epoch-coalescing untested; binary crate has 0 tests.

## MEDIUM (fix the impactful ones)

- rv-lsp F4 TOCTOU re-reads per query; F5 stale-diag conversion; F6 PushDiagnostics never marked.
- rv-git G2 suppressBlankEmpty; G3 diff.submodule=log; G4 fsmonitor daemon; G6 GIT_DIFF_OPTS.
- rv-perf M1 tree watcher counts .git/**; M3 stale AI pane; M4 blocking std::fs in async;
  M5 status/infer_base x3; M6 TUI blocks on bounded action send; M7 scope never reaches
  dispatcher + snapshot clobbers view state; M8 refresh errors publish nothing (NoBase hangs).
- rv-ui M1 spinner never shows; M2 LsStatus frozen; M4 n/N doesn't scroll; M6 view state
  clobbered; M7 AI controls misreport; M9 wrap-vs-scroll; M10 boot blocks on gopls.
- rv-tests T3 kill-escalation path untested; T4 fake-lsp never wired to real client.

## LOW — documented, not all fixed this pass.
