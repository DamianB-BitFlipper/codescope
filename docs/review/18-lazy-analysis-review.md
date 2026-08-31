# Review 18 — lazy per-file semantic analysis

Reviewed commit `0c6e7da` against `cdd5f30`, with `28896ea` as additional
context. This was a static audit of the full diff. Per the request, I did not run Cargo
tests (the supplied baseline is 610 passing tests).

**Counts:** BLOCKER 0 · MAJOR 7 · MINOR 8 · NIT 2

The interactive pipeline does stop at the Git phase, and the actual Git, LSP, and AI
requests are spawned rather than awaited by the dispatcher. The change is not ready to
ship, however: an epoch can currently name old Git facts, old and new same-file jobs can
corrupt the language-server overlay, and the targetless toggle protocol can analyze a
file that the user did not expand.

## BLOCKER

None.

## MAJOR

1. **`crates/codescope/src/dispatcher.rs:532-564,572-620,658-721` — the new epoch can analyze and validate the old fact bundle.**
   `spawn_refresh` advances `self.epoch` but deliberately retains the previous
   `changeset` and `repo_ctx` while Git refreshes. Tab or `A` during that window clones
   those old inputs, tags them with the new epoch, and, for file analysis, even combines
   an old `FileChange` with the new `self.scope`. A result for a path present in both the
   old and new changesets passes both the epoch gate and the path-only membership gate.
   It can therefore publish old hunk/base mappings or an old AI plan as fresh. The result
   can also arrive before `ChangesetReady`, pass membership against the retained old set,
   and remain cached after the new set lands. **Fix:** keep stale display data separate
   from the current job-input bundle. Tag accepted `{repo_ctx, changeset}` with its
   producing epoch and refuse/queue file and AI work until that data epoch equals
   `self.epoch`. Put a changeset generation or input fingerprint in each completion and
   validate it in addition to path membership.

2. **`crates/codescope/src/dispatcher.rs:560-563,633-759`; `crates/codescope-lsp/src/gopls.rs:298-326`; `crates/codescope-lsp/src/rust_analyzer.rs:286-310` — refresh forgets running jobs, so the bound and same-file exclusion do not survive an epoch.**
   Clearing `analysis_in_flight` does not cancel the spawned futures. The new epoch may
   start four more jobs, including the same path. An old completion then removes the new
   path from the set *before* checking its epoch and drains the current queue early, so
   `MAX_FILE_JOBS` is not a bound on actual work. This is more than accounting: base-tree
   analysis temporarily reopens the real file URI with base content and later restores
   disk content. Old/new jobs for the same file can interleave those reopen/query/restore
   sequences and make the current job observe the other epoch's overlay. **Fix:** retain
   an exact `(path, epoch, job_id)` live-job ledger and remove only the matching job;
   enforce the real global limit with a permit held by the task, and serialize a path
   across epochs. Clear pending requests on refresh, not live-task ownership. Do not
   abort an overlay future unless the adapter first makes restoration cancellation-safe.

3. **`crates/codescope-analysis/src/engine.rs:296-370`; `crates/codescope-analysis/src/changes.rs:104-112,153-177`; `crates/codescope/src/dispatcher.rs:722-729` — a transient semantic failure becomes authoritative `Ready` data and can report every base symbol as deleted.**
   `analyse_file` represents a failed worktree symbol query as `worktree: None` plus a
   note. `changed_symbols_detailed` cannot distinguish that from a genuinely deleted
   file, substitutes an empty worktree, and tree-diffs it against a successful base tree.
   For a modified file, all base symbols are then emitted as deletions. Partial/flat
   document-symbol evidence can similarly create deletion claims for omitted children.
   `analyze_changed_file` returns `Ok`, and the dispatcher marks every non-unsupported
   `Ok` as `Ready`; neither the UI nor AI sees the degradation notes. **Fix:** carry typed
   per-side availability/completeness in `FileSemanticResult`. Substitute an empty
   worktree only when the Git status is actually `Deleted`; suppress cross-tree
   add/delete inference when a required query failed. Publish such a job as `Failed` or a
   new `Partial` state and surface its notes instead of treating its count as exact.

4. **`crates/codescope-tui/src/action.rs:22-25`; `crates/codescope-tui/src/run.rs:202-215`; `crates/codescope/src/dispatcher.rs:764-787,1530-1549` — `ToggleFileAnalysis` has no target and can analyze the wrong file.**
   The command depends on a separately delivered, latest-wins `SelectionChanged`.
   Suppose the dispatcher knows A and a fast input burst queues
   `SelectionChanged(B), ToggleFileAnalysis, SelectionChanged(C)`. The dispatcher drops
   the first selection update, toggles A, then selects C; the user expanded B, but A gets
   the LSP job. This directly violates “no per-file LSP work unless the user expands.” A
   retained/unknown selection during a file-list change has the same problem. **Fix:**
   replace the targetless toggle with an idempotent command such as
   `SetFileExpanded { path, expanded, changeset_generation }`, resolved before local
   mutation. Validate the target against that generation and send nothing when there is
   no row. Do not coalesce a selection update across a one-shot command that depends on
   it.

5. **`crates/codescope-tui/src/app.rs:123-141,421-431,584-595`; `crates/codescope-tui/src/run.rs:65-70,262-264` — snapshot replacement preserves a numeric ordinal, not the selected entity.**
   If expanded A is `Loading` and file B is selected at flat index 1, A becoming `Ready`
   with symbols makes index 1 mean A's first symbol. `selection.sync` then retargets the
   diff and starts relations for A without user input. Collapse has the same identity
   bug: collapsing while A's child is selected merely clamps the child's old ordinal,
   which can now be B, C, or another file's child instead of A's file row. **Fix:** before
   replacing/toggling rows, capture stable `(file, optional symbol name/position)`
   identity. Re-resolve it in the new flattened tree; if a selected child disappears,
   land on its owning file row, and only use a nearby file row if the file itself is gone.

6. **`crates/codescope-tui/src/render.rs:493-517`; `crates/codescope-tui/src/app.rs:584-591` — semantic note rows corrupt the renderer's selectable-row index.**
   App deliberately excludes Loading/Unsupported/Failed/Ready-empty notes from
   `flat_file_rows`, but the renderer increments `flat` for all four “never selectable”
   rows. With Loading A followed by B, App says `file_sel == 1` is B while the renderer
   has advanced to 2, so no row is highlighted and `ListState` no longer scrolls to B.
   More notes make the rest of the list lose its active row. **Fix:** remove the four
   `flat += 1` operations. `items.len()` already translates the logical selectable index
   to the physical list-item index. Add one selection-after-placeholder test per state.

7. **`crates/codescope/src/dispatcher.rs:58-64,580-630,856-870` — AI requests in one repository epoch have no request identity.**
   Repeated `A` presses, a model change followed by `A`, or changed lazy coverage can
   launch multiple requests with the same epoch. Every completion validates against its
   own captured facts, but `on_ai_done` accepts all of them; a slow older request can
   overwrite a newer plan and model choice. Epoch validation therefore does not make the
   outcome latest-wins. **Fix:** assign a monotonic AI request generation, carry it in
   `AiStatus::Loading`, `AiDone`, and stored rows, and apply only the current generation.
   Supersede/cancel the older network request where practical.

## MINOR

1. **`crates/codescope/src/dispatcher.rs:540-569,790-810` — refresh publishes invalidated semantics before clearing them, then leaves expansion in an unusable state.**
   `publish_refreshing` emits the new epoch with old `Ready` symbols and old relations;
   only afterward are the caches cleared, with no cleared snapshot until Git returns.
   Meanwhile `expanded_files` is not cleared or replayed. After the new changeset lands,
   an old expanded row is `expanded + Unloaded`, renders no children, and the first Tab
   only collapses it; a second Tab is needed to analyze. **Fix:** invalidate semantic and
   relation state before the refreshing publish. Then either clear expansion/selected
   symbol on refresh, or preserve expansion intent and enqueue still-present paths only
   after the current-epoch changeset arrives.

2. **`crates/codescope/src/dispatcher.rs:264-270,642-679,762-787`; `crates/codescope-tui/src/render.rs:506-510` — provisional, terminal, and retry states have incorrect transitions.**
   While `LsStatus::Starting`, `engine == None`, so an explicitly expanded supported file
   is marked `Unsupported`; `EngineReady` does not replay that explicit request. A real
   `Unsupported` result is not coalesced and is recomputed on every re-expand. A `Failed`
   row says “Tab to retry,” but the first Tab merely collapses it; on the eventual retry,
   the `Entry::Vacant` writes leave the old state as `Failed` instead of `Loading`.
   **Fix:** add a waiting/pending state while the engine starts and launch only explicitly
   expanded paths on `EngineReady`; make genuine Unsupported terminal for the epoch; and
   overwrite Failed with Loading as soon as retry is accepted (or change the advertised
   retry interaction).

3. **`crates/codescope/src/dispatcher.rs:447-475,701-740,989-1028` — the Ready relation gate has two uncovered transitions.**
   A selection reported from a retained/optimistic symbol row while its current file is
   Loading is correctly not spawned, but `on_file_analysis_done` never starts relations
   when that file becomes Ready. `build_impact` then continues to show Loading with no
   job until the selection moves. Also, a deleted file can be Ready from a base tree only;
   selecting its base-revision symbol passes the gate and sends call hierarchy to a
   nonexistent worktree file/position. **Fix:** on a Ready transition, re-resolve and
   spawn the still-current selected symbol. Gate relation capability on a matching
   worktree-revision symbol and `analysis.worktree.is_some()`; render base-only symbols as
   relation-unavailable.

4. **`crates/codescope-tui/src/app.rs:207,233-243,413-478`; `crates/codescope-tui/src/run.rs:174-181,208-215,260` — other key paths still mutate dispatcher-owned expansion locally.**
   Enter on a collapsed file calls `activate` and expands only App's snapshot; no analysis
   is dispatched. Space in Impact, and `h`/`l` in Impact, likewise fall through to local
   file expansion/collapse without a dispatcher command. The local disclosure can remain
   desynchronized until an unrelated snapshot arrives. **Fix:** remove expansion from
   `Activate` and from non-Files panes, or route every real Files-pane change through the
   targeted set-expanded command from MAJOR 4. App should have no independent expansion
   mutators besides a tracked optimistic intent.

5. **`crates/codescope/src/dispatcher.rs:583-618,1408-1437` — the lazy AI prompt's coverage note is not fully honest, although validation remains fail-closed.**
   Ready diagnostics already exist but `change_digest` receives literal `&[]`; per-file
   degradation notes/completeness are omitted; a graph that was never built is labeled
   `Partial` rather than `Unknown`; and Loading, Failed, and Unsupported all count as “not
   yet analyzed (expand with Tab).” Iterating `HashMap::values()` also randomizes which
   first 50 symbols survive the digest cap. `SnapshotFacts` itself is safely narrow:
   unloaded symbols and all unverified edges fail validation, while current files/hunks
   still validate. **Fix:** iterate files in changeset order; aggregate Ready symbols and
   diagnostics plus typed caveats; report each non-Ready state separately; and mark the
   empty relation graph `Unknown` with “not queried,” not as a partial observed answer.

6. **`crates/codescope/src/dispatcher.rs:803-810,1075-1094,1137-1153`; `crates/codescope-tui/src/render.rs:370-377` — several lazy-state messages still claim knowledge the system does not have.**
   The selected-change column says “0 changed symbols” for Unloaded, Loading,
   Unsupported, and Failed files. The summary collapses Unsupported and Failed into
   “symbols not analyzed.” On first lazy startup, `ChangesetReady` still says “symbol
   analysis still running”; in git-only mode `AnalysisDone` need not clear that message.
   The empty snapshot also says “language server unavailable” even when the engine is
   ready and semantics are merely lazy. **Fix:** match all five file states exhaustively
   in both panes and reserve numeric counts for Ready. Remove the eager-era running text,
   preserve the engine-unavailable warning, and describe the empty aggregate as “lazy
   semantics not loaded.”

7. **`crates/codescope-tui/src/render.rs:493-527,606-615` — semantic note rows use byte length and lose required text at supported widths.**
   `text.len()` overcounts `…` and `—`, so padding is wrong. The helper never truncates;
   at the 30-column focus-only layout, the unsupported, failed, and Ready-empty lines are
   clipped at the border, including the Failed retry cue. Expanded `Unloaded` has no note
   at all, so the guaranteed optimistic frame after Tab shows an open disclosure with a
   blank body before Loading arrives. **Fix:** measure with `UnicodeWidthStr`, truncate to
   `inner_w - indent_width`, pad from display width, and use a short narrow retry label.
   Render expanded Unloaded as pending (or optimistically set it to Loading). Add narrow
   tests for all five states.

8. **`crates/codescope/src/dispatcher.rs:586-621`; `crates/codescope-tui/src/run.rs:202-215`; `crates/codescope/src/main.rs:108` — CPU shaping and bounded command sends remain on interactive loops.**
   LSP/Git/AI I/O is spawned correctly, but cloning all Ready symbols, building/rendering
   the digest and facts, and appending unbounded preview/detail strings happen on the
   dispatcher actor before `tokio::spawn`. The code also skips
   `ChangeDigest::truncate_to_budget`. While that actor is busy, the TUI's awaited sends
   to the bounded 64-entry action channel can suspend input and redraw. **Fix:** build the
   owned AI request payload off the actor (use bounded blocking work when warranted), call
   `truncate_to_budget(DIGEST_DEFAULT_TOKEN_BUDGET)` before rendering, and make
   high-frequency UI intents nonblocking/coalesced rather than awaiting a full control
   channel. The per-file LSP await itself is correctly outside the dispatcher.

## NIT

1. **`crates/codescope-tui/src/render.rs:586-597,2633-2647` — the new no-fake-zero test inspects padding, not the count cell.**
   In the 42-column files pane the count is at x=40 (the existing assertion at
   `render.rs:2003-2004` confirms it), but the new test reads x=38. A regression that put
   `0` back in the real cell would still pass. The nearby comment also says Unloaded shows
   `…` while the implementation intentionally emits a blank. **Fix:** derive the cell
   immediately before the border and assert the full table: Unloaded blank/ellipsis,
   Loading ellipsis, Ready-zero `0`, Ready-nonzero count, Unsupported/Failed blank; align
   the comment with the selected policy.

2. **`crates/codescope-tui/src/render.rs:1514` — help still says “Tab still switches.”**
   Tab no longer switches panes. **Fix:** remove that parenthetical or say that `1/2/3`
   switch panes.

## Audit conclusions

- Within one current fact generation, the queue's pop/remove-Loading/re-spawn sequence is
  serialized by the dispatcher actor; a fresh toggle cannot interleave inside it. The
  correctness failures are the cross-epoch live-job ledger and stale input generation
  described above.
- The epoch and selected-symbol identity checks on relation completion are otherwise
  sound. Collapsing clears the dispatcher's selected symbol and relation rows for that
  file; App's ordinal remapping is what retargets selection incorrectly.
- The production count-cell policy is correct: unknown/unsupported/failed are blank,
  Loading is `…`, and only Ready can render `0`. The test does not currently prove it.
- `run_pipeline` and `EngineReady` perform no eager per-file LSP analysis. Once the
  targetless-command bug is fixed, per-file analysis starts only from explicit expansion;
  queued work may reasonably finish after a later collapse and remain cached.
