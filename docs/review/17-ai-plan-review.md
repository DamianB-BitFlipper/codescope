# 17 — AI Plan bottom-pane review

Reviewed the uncommitted working tree against `HEAD` (`cdd5f30`). This was a static review; I did not run Cargo tests.

**Counts:** BLOCKER 0 · MAJOR 3 · MINOR 4 · NIT 2

The outer pane rectangles are unchanged, the longest tab title is 27 display cells and fits the 28 cells available at the `30`-column minimum, and the row-slice arithmetic guards the empty/zero-height cases. Modal ordering is also correct: help swallows `v`, while an open picker receives it as filter input. The main correctness gaps are plan ownership and AI transition identity.

## BLOCKER

None.

## MAJOR

1. **`crates/codescope-tui/src/render.rs:1139-1157`; `crates/codescope-tui/src/app.rs:343-348` — `semantic` is not a durable AI-plan store, so normal symbol selection hides a valid plan.**
   The new tab assumes `UiSnapshot::semantic` is the plan. In the current producer, however, `crates/codescope/src/dispatcher.rs:710-745` returns selected relation rows first and only exposes AI rows in the alternate branch at `:747-764`. If relations are loaded when AI completes, App consumes `Ready` with `ai_generated == false`, clears its loading edge, and never auto-switches when the same Ready plan becomes visible later. If a plan is already open, selecting a symbol changes `semantic` back to deterministic rows; `bottom_view` remains `AiPlan`, the real plan disappears, and deterministic Impact is hidden behind an empty-state message. **Fix:** give `UiSnapshot` a dedicated, epoch-tagged `ai_plan` pane populated independently of selection relations, or make `semantic` AI-only now that `impact` owns deterministic relations. Drive both rendering and transition detection from that durable field. Add a producer-to-App test with a ready plan and loaded `selected_relations`; the new tests only hand-build an AI `SemanticPane`.

2. **`crates/codescope-tui/src/app.rs:80-82,163-173`; `crates/codescope-tui/src/run.rs:21,60-67` — the auto-switch tracks adjacent observed variants, not a request/epoch identity.**
   `prev_ai_loading` discards `Loading::since_epoch` and `Ready::epoch`. Thus a `Loading(E2) -> Ready(E1)` replay can arm the switch, and a Ready whose epoch does not equal `snapshot.epoch` is not rejected. Conversely, the runtime uses a latest-wins watch channel, so a fast `Loading -> Ready` can be coalesced into one observed Ready and the switch is missed. Identical Ready republishes do not double-fire today, but only because the first Ready clears the boolean; there is no durable identity for coalescing, reordered frames, or multiple requests in one repo epoch. **Fix:** publish a monotonic AI request/plan generation with both Loading and Ready, remember the pending/handled generation, and switch on the first current, renderable plan for that generation. At minimum, store `Option<Epoch>` and require `loading_epoch == ready_epoch == snapshot.epoch`, but an epoch alone cannot distinguish repeated AI refreshes in the same repo state. Add coalesced-Ready, mismatched-epoch, and delayed-old-Ready tests.

3. **`crates/codescope-tui/src/app.rs:174-180` — a stale `Ready` status strands the UI on the unavailable AI tab.**
   This is a concrete current path, not only malformed input. `crates/codescope/src/dispatcher.rs:484-497` advances the repo epoch on scope/base/manual refresh without always changing an existing `AiStatus::Ready`; `panes()` then emits the old plan as empty rows plus the stale note (`:747-760`). Because `plan_gone` checks only the enum variant and excludes `Ready { epoch: old }`, an auto-opened AI view stays active indefinitely and hides usable deterministic Impact. **Fix:** treat `Ready { epoch }` (and `Loading { since_epoch }`) with an epoch different from `snapshot.epoch` as stale when no generated plan is present, switch to Impact, and reset scroll. Also centralize the producer's epoch bump so it publishes `Stale` for every invalidated plan.

## MINOR

1. **`crates/codescope-tui/src/render.rs:1144-1151` — empty-state precedence reports the wrong AI state and can display an unrelated deterministic note.**
   Both “this is not an AI pane” and “a current AI pane is empty” enter one branch, and any `SemanticPane.note` wins before `AiStatus` is examined. Opening the tab during initial Loading therefore says `AI returned no renderable rows`; Disabled plus a partial deterministic graph can show `partial: some relationships unavailable`; Failed/Stale without a note also look like an empty successful response. **Fix:** match a typed AI presentation state first: Loading → generating, Disabled/Idle → unavailable, Failed → failed/fallback, Stale → the stale AI note, and only current Ready + `ai_generated` + empty rows → “no renderable rows.” Do not reuse a note whose pane is not AI-generated.

2. **`crates/codescope-tui/src/render.rs:1159-1166,1185-1198` — model-controlled titles and labels are not width-budgeted, so required suffixes disappear.**
   Ratatui clips safely by display cells, so this is not a UTF-8 panic, but it clips the right edge after the unbounded title/label. A long CJK/emoji label or title can hide the ` AI` badge, relation, note, and diagnostic ` !`. **Fix:** reserve display cells for indentation and the required suffixes first, cap indentation, then grapheme-truncate the title/label into the remaining budget with `truncate_cells`/`UnicodeWidthStr`. Add wide, combining, and long-label tests that assert the relation and diagnostic remain visible.

3. **`crates/codescope-tui/src/render.rs:1096,1102-1104` — Loading removes the active-tab cue and violates the palette contract.**
   When `AiPlan` is active, Loading replaces its active style with `MUTED`, so both tabs look inactive. Section 2 of `docs/review/15-redesign-spec.md` assigns loading/stale to `WARN` and restricts `BOLD` to a short list that does not include tabs. **Fix:** keep the active label visibly active, render only the loading ellipsis in `WARN`, and avoid tab bold unless the palette spec is intentionally revised.

4. **`crates/codescope-tui/src/app.rs:1005-1040`; `crates/codescope-tui/src/render.rs:2135-2166` — the new “epoch-matched” test fixtures are actually epoch-mismatched.**
   Both helpers start from a default/sample snapshot at epoch zero, then install Loading/Ready statuses for epoch 1 or 3 without updating `UiSnapshot::epoch`. They therefore bless the missing freshness checks above and would fail after the correct guard is added. **Fix:** set each fixture's snapshot epoch to the status/plan epoch and add a separate negative fixture whose epochs intentionally differ.

## NIT

1. **`crates/codescope-tui/src/render.rs:2210-2223` — the tab-style test converts a UTF-8 byte offset into a terminal x coordinate.**
   `row_text(...).find(...)` returns bytes, while the leading `┌` is three bytes but one cell. The assertions inspect cells two columns to the right and pass only because the whole spans currently share a style. **Fix:** locate the label by buffer-cell symbols or convert the prefix to display width before calling `cell`.

2. **`crates/codescope-tui/src/render.rs:1079,1094-1115,1360-1368` — the intentional tab UI has no matching redesign-spec update.**
   The outer y/height and Impact's 40/30/30 geometry remain compliant, but §3.5 still requires the exact outer title `Impact`, and §3.7 requires an exact wide help string without `v impact/AI`. Comments cite `docs/review/16`, which is a review rather than an AI Plan design contract. **Fix:** add or update a normative spec to supersede those exact title/help clauses (or, if §15 remains authoritative, do not replace the permanent Impact body/title).
