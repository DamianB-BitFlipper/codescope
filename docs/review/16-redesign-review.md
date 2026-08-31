# 16 — TUI redesign review

Reviewed commit `28896ea984f98d4e1cb0251e020a98d6f52363c9` against
`docs/review/15-redesign-spec.md`. This was a static review; per request, I did not run
`cargo test`.

**Counts:** BLOCKER 0 · MAJOR 6 · MINOR 12 · NIT 5

The normal/focus-only tier thresholds, six-row outer stack, `80x20` split, `28..=56`
files-width preference, nine-row Impact pane, basic `2 * ln_width + 5` calculation,
selection-over-owner background precedence, focus cycle, direct `RelationsLoaded`
epoch/identity comparison, and LSP evidence completeness mapping are correct. I did not
find a byte-slicing panic in the intraline path: its byte offsets originate at UTF-8
boundaries and are applied by grapheme overlap. The findings below are the remaining
correctness and contract gaps.

## BLOCKER

None.

## MAJOR

1. **`crates/codescope/src/dispatcher.rs:468-499, 527-557, 597-601, 725-750` — a refresh publishes old analysis and relation rows as if they belonged to the new epoch.**
   `spawn_refresh` advances the epoch and clears only `repo_ctx`/`changeset`; it keeps
   `analysis` and `selected_relations`. `publish_refreshing` therefore combines old files
   and `Ready` Impact rows with the new `UiSnapshot.epoch` (and an empty diff). On failure,
   those old facts remain indefinitely. It also starts `spawn_expand` immediately, before
   the new analysis/LSP refresh finishes, so an old-position relation answer can be tagged
   with and accepted for the new epoch. The event apply gate itself is exact, but this
   sequencing bypasses the guarantee it is meant to provide. **Fix:** epoch-tag or clear
   every derived store at refresh start, move selected lists to `Loading`, and launch the
   relation query only after the matching `AnalysisDone` is accepted. Keep an old frame
   only with an explicit stale/data epoch; otherwise clear the whole dependent frame
   atomically.

2. **`crates/codescope/src/dispatcher.rs:213-226, 546-551, 774-800` — a successful git-only scan falsely reports `LSP ✓` and erases the git-only warning.**
   `run_pipeline` intentionally returns `Ok(git_only_snapshot(...))` when there is no
   engine, but every `Ok` sets `ls_status = Ready` and resets the typed status. This races
   with `EngineUnavailable` at startup and happens deterministically after a later manual
   refresh, leaving Impact `Unavailable` while the top bar says the LSP is ready. **Fix:**
   set `Ready` only when `self.engine.is_some()`. Preserve `Failed`/`Starting` and the
   git-only Warning for a git-only result (ideally track status sources rather than clearing
   every status on any successful scan).

3. **`crates/codescope-tui/src/app.rs:117-132, 484-494`; `crates/codescope/src/dispatcher.rs:476-479` — snapshot refreshes defeat App-owned hunk state.**
   A normal refresh publishes a transient default diff (`path -> "" -> path`), so
   `App::update` treats it as two real retargets and resets vertical/horizontal scroll and
   `current_hunk` to the top. If a latest-wins update skips that transient frame but changes
   same-file row/header offsets, `clamp` preserves a numerically valid `current_hunk`
   without recomputing it from the new scroll anchor. Summary and title can then disagree
   with the visible hunk. **Fix:** preserve a stable selected-file identity while refreshing
   and reset only on a real file retarget. After installing same-file rows, either re-anchor
   to the preserved hunk or call `sync_current_hunk` after scroll clamping.

4. **`crates/codescope-tui/src/intraline.rs:41-72, 164-172` — replacement rows are zipped positionally instead of receiving the required monotonic line alignment.**
   An insertion or exact-line anchor inside a delete/add run shifts every later partner,
   which suppresses the real highlight and can brighten unrelated lines. For example,
   old `["let keep = 1", "return old"]` versus new
   `["let inserted = 0", "let keep = 1", "return new"]` never pairs the two `return`
   lines. **Fix:** run `TextDiff::from_lines` over each maximal delete-then-add run, then zip
   rows by relative order only inside each `Replace` operation; leave equal/one-sided rows
   unpaired.

5. **`crates/codescope-tui/src/render.rs:883-916, 940-987, 1000-1030` — tabs are measured as expanded cells but emitted as literal control characters, so they disappear.**
   `grapheme_cells` charges a tab to a four-cell stop, while the styled spans and wrapped
   strings retain `\t`. Ratatui filters control graphemes rather than advancing a tab stop.
   Common tab-indented source therefore shifts left, wrap points are wrong, and raw `x+NN`
   can claim movement without changing the visible body. **Fix:** replace each tab with the
   correct number of styled spaces while constructing visual graphemes. Use those expanded
   cells for measurement, wrapping, slicing, and output.

6. **`crates/codescope-tui/src/app.rs:273-297, 371-390, 484-487`; `crates/codescope-tui/src/render.rs:777, 828` — valid diffs beyond 65,535 logical/visual rows wrap navigation back toward the start.**
   Row/header indices and wrapped visual offsets are narrowed with `as u16`. A large
   generated-file diff makes `Down`, `G`, or `n` wrap the anchor while `current_hunk`
   advances, violating their shared-state invariant. A sufficiently large public
   `total_hunks` also wraps through `i32` before `clamp`. **Fix:** keep logical rows, hunk
   arithmetic, and visual offsets as `usize`. Pre-slice/window the rendered lines at the
   logical anchor before calling Ratatui, or impose an explicit visible-row cap with a
   truncation notice; do not silently narrow.

## MINOR

1. **`crates/codescope-tui/src/app.rs:152-180, 320-359` — Impact-focused tree keys mutate the hidden Files pane.**
   `Space` is unconditional, and `h`/Left/`l`/Right send every non-Diff pane through the
   files-tree collapse/expand helpers. In focus-only or zoomed Impact this can collapse an
   invisible owner, clamp the numeric row onto another file, and retarget the Impact being
   viewed. **Fix:** make these actions operate only on `Pane::Files`; keep raw h-scroll only
   on `Pane::Diff` and no-op on cursorless `Pane::Impact`.

2. **`crates/codescope-tui/src/intraline.rs:83-89, 126-140, 164-167, 185-228` — the word algorithm is not the specified `TextDiff::from_words`, and its safety caps do not bound total redraw work.**
   The custom word/space/punctuation tokenizer produces observably different spans (for
   example, `foo(1)` -> `foo(2)` highlights only the numeral instead of the changed
   non-whitespace word token). A 41-row block drops all highlights while arbitrarily many
   smaller blocks are still rediffed on every frame. **Fix:** use `TextDiff::from_words` as
   specified, apply a work budget to aligned replace candidates rather than blanking an
   entire block at one cliff, and cache or bound aggregate per-frame work.

3. **`crates/codescope-tui/src/render.rs:957-987` — raw h-scroll is not an absolute display-cell window when the left edge bisects a wide grapheme.**
   For `界ab`, `skip=1`, `budget=2`, the code drops `界` but emits both `a` and `b`, leaking
   one cell past `[1, 3)` and shifting the content left. It remains UTF-8 safe, but `x+NN`
   is not the window it reports. **Fix:** charge the hidden remainder of a straddled
   grapheme against the viewport (or emit equivalent padding) before admitting later
   graphemes.

4. **`crates/codescope-tui/src/render.rs:637-652, 855-864` — `ln_width <= 6` does not cap the rendered line-number fields.**
   Rust formatting width is a minimum, so line `1_000_000` takes seven cells while body and
   h-scroll math still assume a six-cell field. Source cells at the right can become
   unreachable. **Fix:** render every number into exactly `ln_width` cells (for example,
   a leading-ellipsis six-cell form), or change the geometry consistently rather than
   allowing per-row gutter growth.

5. **`crates/codescope-tui/src/render.rs:577-581, 621-629, 855-916` — add/remove change glyphs and diff signs use the wrong palette tokens.**
   Nested `+`/`-` glyphs fall through `status_style` to WARN, and diff signs receive
   `ADD_BODY`/`DEL_BODY` (`TEXT` foreground) rather than `ADD_FG`/`DEL_FG`. **Fix:** define
   explicit added/removed sign/glyph styles and apply the requested accent to the sign (and
   gutter where required) while keeping source body backgrounds separate.

6. **`crates/codescope-tui/src/render.rs:695-701`; `crates/codescope/src/dispatcher.rs:643-649, 1058-1082` — focused-symbol identity is softened in two places.**
   The renderer's App fallback can label an old file diff with a newly selected symbol from
   another file while dispatch catches up. The dispatcher also falls back from
   `(file, name, line, col)` to `(file, name)`, which can select the wrong overload/duplicate
   after a refresh. **Fix:** use the published `focused_symbol` only, or at least require
   `app.selected_file_path() == diff.title`; resolve deterministic selection with the full
   tuple and clear/re-request a selection that no longer matches exactly.

7. **`crates/codescope/src/dispatcher.rs:207-212, 272-278, 439-479` — AI `Ready` is not changed to `Stale` on most epoch-changing refresh paths.**
   Manual refresh, scope/base changes, and `EngineReady` call `spawn_refresh` directly, so
   the top bar can remain `AI ✓` for an old-epoch plan. `RepoChanged` takes a separate path
   that increments twice and records the intermediate epoch in `AiStatus::Stale`. **Fix:**
   centralize the single epoch bump and AI stale transition inside `spawn_refresh` (or one
   shared transition helper).

8. **`crates/codescope/src/dispatcher.rs:565-588` — AI failure/recovery status transitions are incomplete.**
   `AiOutcome::Unavailable` falls through to `Idle` with no Warning or required
   retry/model/deterministic-fallback suffix. Conversely, a successful retry sets `Ready`
   but leaves the previous AI failure text in the status bar. **Fix:** map Unavailable and
   non-renderable outcomes to a sanitized Warning with the suffix, and clear an earlier
   AI-owned failure status when a current-epoch plan succeeds.

9. **`crates/codescope-tui/src/render.rs:379-407` — the constrained summary fallback can return a string wider than its area and clip the protected hunk phrase.**
   Once `"N changed files · hunk C / T"` is already too wide, the `parts.len() > 2` branch
   re-inserts an ellipsized middle phrase, making the result even wider. The final string is
   not width-bounded. **Fix:** never reinsert the middle after the ends fail to fit; allocate
   the hunk phrase first, elide the count as needed, and enforce `result.width() <= width`
   (also avoid subtracting the leading-space budget twice).

10. **`crates/codescope-tui/src/render.rs:713-745` — valid 30-column zoom/raw Diff titles can overwrite the basename.**
    The symbol is the only removable part; the remaining right title is never bounded and
    `base_budget.max(1)` forces a left title anyway. At width 30,
    `" hunk 1/1 · wrap off · ZOOM "` already occupies all 28 inner cells. **Fix:** budget
    both titles against the usable border width, drop lower-priority `x`/zoom adornments as
    needed, and emit no left title when no separated cell remains.

11. **`crates/codescope-tui/src/render.rs:214-242, 270-273` — narrow right-reserved chrome can clip the refresh indicator.**
    The whole right group is capped to terminal width but rendered left-to-right with `⟳`
    last. At width 30, `branch  LSP ✓  AI × anthropic  ⟳` loses the refresh state, contrary
    to its priority rule. **Fix:** reserve/render the critical service glyphs and spinner
    from the right first; elide provider and then scope before clipping failure/refresh
    state.

12. **`crates/codescope-tui/src/render.rs:1157-1215` — long Impact labels clip the required relation suffix and diagnostic badge.**
    Rows append relation/`!` after an unbounded label and rely on Paragraph clipping, so
    the semantic qualifier disappears first. **Fix:** reserve cells for relation and
    diagnostic suffixes, then grapheme-elide the label to the remaining width.

## NIT

1. **`crates/codescope-tui/src/render.rs:779-785` — wrapped hunk headers ignore the prefix-fit rule.**
   Generic wrapping can split the `@@ ... @@` prefix itself. **Fix:** find and measure the
   closing `@@`; if that prefix does not fit, render one truncated full-width band instead
   of wrapping it.

2. **`crates/codescope-tui/src/snapshot.rs:164-176`; `crates/codescope/src/dispatcher.rs:933-940` — hunk ownership remains duplicated.**
   `DiffPane.current_hunk` is still public and perpetually rebuilt as 1 even though App owns
   the live value. **Fix:** remove the snapshot field after migration so a future renderer
   cannot accidentally reintroduce the reset bug.

3. **`crates/codescope/src/dispatcher.rs:750-752` — incomplete graph evidence sets only a pane note, not the typed list flags.**
   `ImpactList.partial` remains false even though incomplete graph evidence contributes to
   both lists. **Fix:** propagate graph incompleteness into the affected list flags (or add
   a separate typed graph-completeness field) and derive the note from those typed values.

4. **`crates/codescope-tui/src/elide.rs:25-27, 63-74` — the tiny-budget collision fallback violates its own width/uniqueness contract.**
   Budgets below three return the same `…` for every path; at budget three, reserving
   `max(1)` base cell plus `·NN` emits four cells, and ordinal 100 is wider than the fixed
   three-cell reservation. **Fix:** reserve the actual ordinal width, use a fixed-width
   identity fallback when no path fragment fits, and run a final uniqueness/width check.

5. **`crates/codescope-tui/src/render.rs:1254-1288` — the wide help text is not the exact contracted string.**
   It renders `[/] resize` instead of `[ / ] resize`. **Fix:** use separate spaced key text
   (or the exact literal) at `width >= 96`.
