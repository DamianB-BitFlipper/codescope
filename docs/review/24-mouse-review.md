# Review 24 — mouse interaction implementation

Read-only review of the uncommitted mouse-interaction work against
`docs/review/23-mouse-ux.md`. I changed no Rust source and ran no Cargo command. This document is
the only file I wrote.

## Recommendation

**Do not merge yet.** Normal-mode file clicks mostly follow the intended model, and the terminal
cleanup guard is statically well ordered. However, the renderer discards the geometry it builds for
mouse input. Impact-height dragging therefore does not resize the rendered pane and immediately
makes hit targets disagree with the pixels. All-motion mouse capture also feeds the existing biased,
unconditional-redraw loop, so pointer movement can starve snapshots and ticks.

## Findings

### BLOCKER

#### B1. The retained geometry is not the geometry rendered, and Impact dragging only changes invisible state

**Locations:** `crates/codescope-tui/src/run.rs:44-47`,
`crates/codescope-tui/src/render.rs:71-100`,
`crates/codescope-tui/src/geometry.rs:53-104`,
`crates/codescope-tui/src/layout.rs:73-77`

`run` builds a `UiGeometry`, then calls the old renderer without it. The renderer independently
repeats every layout and still uses the constant `IMPACT_HEIGHT` (9). Geometry instead gives the
Impact constraint `app.impact_height`, and the new `impact_height()` dynamic clamp is unused.
Consequences:

- `SetImpactHeight` changes `App`, but never changes the pane the user sees.
- On the next draw, hit rectangles move even though pixels do not. At 140x40 with a request of 18,
  geometry places Impact at about y=20 with height 18, while rendering leaves it at y=29 with height
  9. Visually rendered work rows can therefore focus/hit Impact.
- The horizontal handle moves away from the rendered boundary after the first effective drag.
- The claimed “one frame, one geometry owner” invariant is false. Files and focus-only layouts are
  also duplicated and can drift.
- Rendering still accepts a second `UiSnapshot` and clones `app.snapshot`, so pixels and hits are not
  forced by the API to describe one snapshot.

**Concrete fix:** compute the complete frame plan once in the draw closure. Use
`impact_height(app.impact_height, area.height)` for the effective normal-layout height. Change
`render` and its pane functions to consume that exact `UiGeometry` and `app.snapshot`; remove their
private `Layout` splits and the second snapshot argument. Retain that same successfully rendered
geometry for input.

#### B2. All-motion capture can cause unbounded redraw and starve state updates

**Locations:** `crates/codescope-tui/src/run.rs:43-56`,
`crates/codescope-tui/src/run.rs:63-85`, `crates/codescope-tui/src/run.rs:89-103`

`EnableMouseCapture` enables motion events, but the loop is still `biased` toward `EventStream` and
draws unconditionally before every select. `MouseOutcome::dirty` is never read. A stream of ignored
`Moved` events can therefore redraw as fast as input arrives while preventing snapshot and tick arms
from running. Two closed-source paths are also permanently ready: event EOF/error is ignored, and a
closed watch receiver executes `continue` and polls `changed()` again. Either can become a hot draw
loop.

**Concrete fix:** add a run-local `dirty` flag and draw only when it is set. Set it for keys,
snapshots, resize, ticks that animate, and dirty mouse outcomes; leave it clear for `Moved` and other
no-ops. Remove `biased` (or enforce an explicit event budget). On event EOF/error and watch closure,
return or mark that arm closed so it is no longer selected.

### MAJOR

#### M1. Focus-only geometry covers chrome and has no row or tab targets

**Location:** `crates/codescope-tui/src/geometry.rs:67-84`

The renderer keeps top, summary, status, and sometimes help rows, and renders the focused pane only
in the body (`render.rs:104-134`). Geometry instead assigns the focused pane the entire terminal.
It then returns before building `file_row_rects` or bottom tabs. Thus:

- chrome clicks are incorrectly reported as pane clicks;
- files cannot be selected by mouse in compact fallback or Files zoom;
- Impact/AI Plan labels cannot be clicked in Impact fallback or zoom;
- `pane_at` does not describe the frame the user saw.

Hidden panes and handles are correctly `None`, but the one visible pane is wrong.

**Concrete fix:** put the two focus-only chrome stacks in the shared frame plan and assign only the
actual body rectangle to the focused pane. After tier layout, run common population code for a
visible Files viewport and visible Impact tabs. Render those rectangles rather than recomputing
them.

#### M2. Both divider targets are one cell instead of the specified adjacent two cells

**Location:** `crates/codescope-tui/src/geometry.rs:103-111`

The normal files/diff boundary has two visible border columns (for example x=41 and x=42 at
140x40). The vertical handle covers only x=41 and includes the work bottom-border row. The work and
Impact blocks similarly have two adjacent border rows (y=28 and y=29 by default), but the horizontal
handle covers only y=28. As a result, the Diff left border does not arm a vertical drag, the Impact
top border outside a label does not arm a horizontal drag, and tab/handle overlap precedence never
occurs in real geometry.

**Concrete fix:** make the vertical handle width 2 around `diff.outer.x` and stop it before the work
bottom border. Make the horizontal handle height 2 over the work bottom plus Impact top borders.
Keep tab routing before the horizontal handle. Derive these rectangles from the shared rendered
pane rectangles.

#### M3. The drag transition loses final samples, emits setters without movement, and survives removed handles

**Locations:** `crates/codescope-tui/src/mouse.rs:88-106`,
`crates/codescope-tui/src/mouse.rs:194-262`,
`crates/codescope-tui/src/run.rs:58-83`

There are three state-machine failures:

1. Every left `Drag` returns a resize action before checking `did_move`. A same-coordinate Drag can
   overwrite a constrained stored preference with the effective extent (for example stored 42 to
   effective 32 at width 80), even though `dirty` is false.
2. `Up` samples its coordinate only when an earlier Drag set `moved`. A Down followed directly by an
   Up at a different coordinate ends the gesture without committing the release position. The
   exact same-coordinate Down/Up case is correctly a no-op, and Up outside works only after at least
   one prior moved Drag.
3. `route_drag` ignores `geometry`. If `z` removes the handle, or a modal is opened and closed using
   keys before another mouse event, the old drag remains armed and a later Drag/Up resizes a hidden
   pane. Resize itself does cancel the drag correctly.

The signed formula and Impact y direction are otherwise correct (up makes Impact taller). The final
`as u16` can also wrap for an extreme positive signed sample instead of saturating.

**Concrete fix:** use one signed, saturating sample helper for both Drag and Up. Emit on Drag only
when the relevant pointer coordinate changed. On Up, compare the release coordinate with the start
as well as prior movement, apply that final sample when needed, and always return Idle. Before
routing an active drag, cancel it if its boundary's handle is absent; also cancel immediately after
a key action opens a modal or removes the handle. Clamp in the signed domain before converting to
`u16`.

#### M4. `file_rows` is not authoritative, so public non-Ready snapshots still create hidden keyboard selections

**Locations:** `crates/codescope-tui/src/file_rows.rs:18-126`,
`crates/codescope-tui/src/render.rs:477-551`,
`crates/codescope-tui/src/app.rs:540-648`,
`crates/codescope-tui/src/run.rs:175-191`

The new projection correctly gives expanded non-Ready/Ready-empty files one non-selectable note and
gives Ready symbols logical indices. Rendering, however, still constructs a separate row vector.
The projection has no `Empty` row even though the renderer draws one. More importantly, App lookup,
clamping, toggle, restore, and selection-dispatch helpers still count `f.symbols` for every expanded
file regardless of semantic state.

For an expanded `Failed`/`Loading`/`Unloaded` public `FileRow` that retains symbols, pixels and mouse
show `File + Note`, while arrows can select the hidden symbols and `SelectionTracker` can dispatch
them. `logical_row_count` and `resolve_logical`, which have the correct semantic rule, are currently
unused. The new module therefore does not close the invariant hole identified by the design.

**Concrete fix:** add explicit semantic note kinds and `Empty` to the projection. Build rendered
items from that projection, rather than with a second tree walk. Delegate App row count/resolution,
selection restoration/toggle helpers, and run-loop selection resolution to the same projection.
Ensure all Ready-only rules are shared.

#### M5. Only 1 of the 24 planned acceptance tests is fully covered; the terminal panic proof bypasses this implementation

**Locations:** `crates/codescope-tui/src/mouse.rs:270-509`,
`crates/codescope-tui/src/file_rows.rs:1-126`,
`crates/codescope-tui/src/geometry.rs:1-192`,
`crates/codescope/src/main.rs:65-70`,
`crates/codescope/tests/terminal_restore.rs:9-67`

The new row and geometry modules have no tests. Mouse tests cover happy-path actions but not dispatch,
physical scrolling, boundary cells, clamps, ignored kinds, or lifecycle removal. The existing PTY
panic hook initializes ratatui directly, so it never enables mouse capture, never exercises
`run_with_terminal`, and asserts only alternate-screen restoration. There are no injectable guard
ordering/enable-failure tests and no dropped-future test.

**Concrete fix:** implement the plan summarized below. In particular, route
`CODESCOPE_TEST_PANIC` through `run_with_terminal`, assert mouse enable and reverse-disable escape
sequences, add injectable terminal operations for the ordering matrix, and add a pending-future
drop/cancellation test.

### MINOR

#### m1. `first_visible` violates the zero-capacity contract

**Locations:** `crates/codescope-tui/src/file_rows.rs:96-105`,
`crates/codescope-tui/src/render.rs:562-568`

For non-empty rows and `capacity == 0`, `first_visible` can return a nonzero selected offset, while
the design requires zero. Rendering then compensates with `.take(capacity.max(1))`, so it does not
render the exact zero-capacity slice. Current usable pane bodies normally have positive capacity,
but this is an edge invariant and a planned test case.

**Concrete fix:** return zero immediately when capacity is zero (and when there is no selected
physical row), and use `.take(capacity)`.

#### m2. The loading decoration is clickable as part of `AI Plan`

**Locations:** `crates/codescope-tui/src/geometry.rs:164-189`,
`crates/codescope-tui/src/render.rs:1304-1328`

For loading state, geometry changes the target text to `"AI Plan …"`. The renderer keeps the label
`"AI Plan"` and adds `" …"` as a separate decoration. Geometry therefore makes both the suffix
space and ellipsis clickable, contrary to the contract. The ordinary `Impact` and `AI Plan` letter
rectangles otherwise start at the correct rendered cells.

**Concrete fix:** make one bottom-title descriptor own spans and cell widths for both render and
geometry. Keep the AI target width equal to `"AI Plan"` in every status; exclude the separator,
space, ellipsis, and zoom suffix.

#### m3. Raw-mode ownership is still duplicated in the run loop

**Location:** `crates/codescope-tui/src/run.rs:31-33`

`run` still calls `enable_raw_mode()` and ignores failure even though `run_with_terminal` owns raw
mode, alternate screen, and mouse capture. This weakens the lifecycle boundary and makes the
library loop mutate process terminal state unexpectedly.

**Concrete fix:** remove this call. Keep all entry and cleanup operations in `terminal.rs`.

#### m4. Mouse controls remain undiscoverable and the help text retains a known false statement

**Locations:** `crates/codescope-tui/src/render.rs:1600-1647`,
`crates/codescope-tui/src/render.rs:1652-1679`

The footer does not mention click or drag. The modal still says “keyboard controls”, has no mouse
rows, and says `z ... (Tab still switches)` although Tab analyzes/expands only in Files and never
switches panes.

**Concrete fix:** use the review-23 footer wording, rename the heading to “controls”, add click and
drag rows, and remove/correct the Tab claim. Do not advertise wheel.

### NIT

#### n1. The structural row model is exposed as public API unnecessarily

**Location:** `crates/codescope-tui/src/lib.rs:12`

`file_rows` is an implementation detail, and no public geometry field exposes its types. Making the
whole module public increases the compatibility surface while the model is still incomplete.

**Concrete fix:** use `mod file_rows;` (or `pub(crate) mod file_rows`) and expose only deliberately
stable geometry/mouse types required by external tests.

## Test-plan audit (review 23 items 1–24)

| Coverage | Plan items | Evidence / gap |
|---|---|---|
| **Present (1)** | **11** | Normal Files focus is covered by the blank-tail test, and Diff/Impact by `click_focuses_each_pane`. |
| **Partial (14)** | **1, 2, 4, 5, 6, 7, 9, 10, 12, 14, 16, 17, 18, 19** | Existing render tests check default pixels for 1/2/4/5 but not `UiGeometry`, handles, dynamic sizes, or targets. Mouse fixtures exercise a Ready symbol and basic row/tab/pane/drag actions, but do not assert the complete projection, rendered-label boundaries, dispatch/sync count, note behavior, idempotent App state, all modals, both border cells, clamps, or outside release. The existing semantic-state render test is visual only. |
| **Missing (9)** | **3, 8, 13, 15, 20, 21, 22, 23, 24** | No Impact clamp/work-preservation table; physical viewport scroll test; note-click sync test; tab/handle precedence test; resize/removal drag lifecycle test; ignored-kind table; terminal guard matrix; real mouse-capture PTY panic proof; or dropped-future cleanup test. |

For item 20, the task prompt asks resize to cancel the drag while review 23 also describes rebasing
when Normal survives. There is no test for either policy; choose the current cancellation policy
explicitly and test it.

## Verified behavior

- Normal-mode projected note rows have no logical target, and a note hit routes to Files focus only.
- For positive viewport capacity, the physical selected-row scroll formula matches review 23.
- A row click is one atomic `SelectFileRow` action. `run` sends it through `dispatch`, whose single
  tail call to `SelectionTracker::sync` is reused; there is no direct `SelectionChanged` emission.
- `SetBottomView` focuses Impact, names the exact view, and resets AI scroll only on a real change.
- The mouse formula uses 0-based crossterm coordinates and the Impact y axis in the correct direction.
- A plain same-coordinate Down/Up does not resize. A prior moved drag releases outside and returns
  Idle. Modal state present at mouse-routing time swallows the event and cancels the drag.
- Resize explicitly sets the run-local drag to Idle. Focus-only geometry exposes no new handles.
- `inner()` applies the correct one-cell offset for the renderer's all-border block in the sizes in
  which it is used.
- `terminal.rs` statically enables capture only after arming the guard. Guard Drop attempts
  `DisableMouseCapture` and then calls `ratatui::restore()` without `?`; an enable error therefore
  also drops the guard. The panic wrapper disables capture before chaining ratatui's hook.
- `map_key` is unchanged. The 1/2/3, Tab, arrows, `[ ]`, and resize mappings remain in place, and no
  pre-existing test body was changed in the diff.
