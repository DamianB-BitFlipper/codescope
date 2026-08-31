# Review 23 — mouse UX and architecture

Static design review of HEAD `075f18bb0c2f758b46b8be7d56a024f2a8f08a56`.
I did not change Rust source and did not run Cargo tests. This document is the only artifact.

## Recommendation

Add mouse support around one **run-loop-owned `UiGeometry` for the last successfully drawn
frame**. Build it once inside the draw closure, pass that exact value to every render function,
and retain it for the next input event. Do not put a geometry cache in `App`, and do not let
`map_mouse` repeat any `Layout` constraints.

Use a shared physical file-row projection for both rendering and hits. It must include file rows,
symbol rows, and non-selectable note/empty rows. Remove the throwaway `ListState` as the hidden
owner of scrolling; compute the first visible physical row in the frame plan and render that exact
slice.

Use left-button down for pane focus, row selection, and direct bottom-tab selection. Use a
run-local drag state for the two dividers. Apply absolute, clamped sizes live during a drag and end
on left-button up anywhere. Add `App::impact_height`, defaulting to 9. Defer wheel support until the
files pane has an independent viewport offset; mapping its wheel to selection would trigger costly
semantic retargets and does not behave like normal scrolling.

Enable mouse capture in the binary's terminal session, not in the TUI run loop. Pair it with an
RAII cleanup guard for normal return, `Err`, and future cancellation, and wrap ratatui's installed
panic hook so a panic disables mouse capture before ratatui restores raw/alternate-screen state.

## Verified current architecture

- `choose_tier` and the files-width policy live in
  `crates/codescope-tui/src/layout.rs:34-65`. The normal threshold is `80x20`; the requested files
  width is clamped to `28..=56` and may yield further to the diff's 48-column minimum.
- The actual rectangles are private to rendering. The normal six-part split and files/diff split
  are constructed in `crates/codescope-tui/src/render.rs:79-103`. Focus-only has separate 5-row and
  4-row layouts at `render.rs:104-134`. `TooSmall` returns before panes or overlays at
  `render.rs:73-77`.
- The files renderer creates its own physical row vector at `render.rs:477-558`. Note rows for
  Loading, Unsupported, Failed, Ready-empty, and Unloaded occupy a displayed line but deliberately
  do not increment the selectable flat index (`render.rs:507-545`). A new local `ListState` maps
  the selected logical row to a physical row and ratatui chooses the visible offset during render;
  that state is then discarded (`render.rs:566-569`).
- `App::file_sel` is a logical flat index over files plus expanded symbols
  (`crates/codescope-tui/src/app.rs:40-41,587-617`). `App::update` preserves selection identity and
  reconciles each published snapshot (`app.rs:123-180`). Several other methods independently walk
  the same tree (`app.rs:501-617`).
- `render` accepts both `&App` and a separate `&UiSnapshot` (`render.rs:71`). Files are drawn from
  the explicit snapshot (`render.rs:449-456,481`), while selection/owner lookup reads
  `app.snapshot` (`render.rs:460-463`). Production passes a clone of the latter
  (`crates/codescope-tui/src/run.rs:39-40`), but the API permits them to differ.
- Bottom tabs are styled spans in the Impact block's top-border title
  (`render.rs:1286-1328`). They have no hit rectangles today.
- The event loop handles only Key and Resize. Mouse falls through the catch-all arm
  (`run.rs:45-57`). `map_key` is pure (`crates/codescope-tui/src/action.rs:128-208`). After every
  dispatch, `SelectionTracker::sync` sends a changed resolved target to the dispatcher
  (`run.rs:106-163,264-266`).
- `run_with_terminal` calls `ratatui::init()`, awaits the app, and restores only after the await
  (`crates/codescope/src/terminal.rs:12-20`). That covers a returned error but not cancellation.
  Ratatui's panic hook does not know about crossterm mouse capture. The current panic test hook also
  bypasses `run_with_terminal` (`crates/codescope/src/main.rs:65-70`).

The prompt's “six-row vertical stack” describes only `Tier::Normal`. Focus-only uses five rows at
height >= 12 and four below that; TooSmall has none. Mouse geometry must preserve all three cases.

## Interaction contract

### Left click

1. A selectable file or symbol line focuses Files and selects that exact logical row. It does not
   expand, activate, or separately send `SelectSymbol`. The existing selection tracker emits the
   same single `SelectionChanged { file, symbol }` that keyboard navigation emits.
2. A note line or blank interior in Files only focuses Files. A note is visible but inert.
3. Any other point inside a visible pane focuses that pane, equivalent to `1`, `2`, or `3`.
4. `Impact` and `AI Plan` labels directly select their named view and focus Impact. Clicking the
   already active label is idempotent; it must not toggle to the other view.
5. Right and middle buttons do nothing. Double click has no special meaning in the first version.

### Routing precedence

Use this order for a left-button down:

1. An open modal consumes the event and cancels an active drag. No click-through.
2. An active drag consumes Drag/Up regardless of the pointer's current rectangle.
3. Bottom-tab label rectangles.
4. Divider handles. At their intersection, the horizontal handle wins.
5. A selectable visible file/symbol row.
6. A visible pane rectangle, for focus only.
7. Chrome, separator text, and unowned cells are inert.

Tab labels must win over the two-row horizontal resize target because both occupy part of the
Impact top border. Note rows must win only as a Files focus target; they never produce a selection.

## One frame, one geometry owner

Add `crates/codescope-tui/src/geometry.rs` (or put the same types in `layout.rs`, but do not split
the computation). A concrete shape is:

```rust
pub struct UiGeometry {
    pub area: Rect,
    pub tier: Tier,
    pub top_bar: Option<Rect>,
    pub summary: Option<Rect>,
    pub work: Option<Rect>,
    pub status: Option<Rect>,
    pub help_bar: Option<Rect>,
    pub files: Option<PaneGeometry>,
    pub diff: Option<PaneGeometry>,
    pub impact: Option<PaneGeometry>,
    pub files_viewport: Option<FileViewport>,
    pub bottom_tabs: Option<BottomTabRects>,
    pub drag_handles: DragHandleRects,
    pub modal: Option<ModalGeometry>,
}

pub struct PaneGeometry {
    pub outer: Rect,
    pub inner: Rect,
}

pub struct BottomTabRects {
    pub impact: Rect,
    pub ai_plan: Rect,
}

pub struct DragHandleRects {
    pub files_diff: Option<Rect>,
    pub work_impact: Option<Rect>,
}
```

`PaneGeometry::inner` must come from the same all-borders contract as `pane_block`
(`render.rs:163-179`), not from repeated `+1/-2` arithmetic in hit code. A shared bottom-title
builder should own both the span strings and their cell widths, so the clickable label rectangles
cannot diverge from `bottom_tab_title` (`render.rs:1303-1328`). The loading ellipsis is decoration,
not part of the AI Plan target.

The run loop should own `last_geometry`:

```rust
let mut next_geometry = None;
terminal.draw(|frame| {
    let geometry = UiGeometry::build(frame.area(), &app);
    render(frame, &app, &geometry);
    next_geometry = Some(geometry);
})?;
let last_geometry = next_geometry.expect("draw closure ran");
// Wait for one event. Mouse routing reads only last_geometry.
```

This gives one computation per frame and ensures input targets the frame the user actually saw.
After a key, snapshot, resize, or mouse action, the loop draws again before handling another event.
Do not recompute geometry from `terminal.size()` in the mouse branch. Do not store it in `App`:
geometry is derived frame state, would become stale on resize/snapshot changes, and does not belong
in the view model. A render-side global/cache is also wrong because the event loop needs explicit
ownership of the last successful draw.

As part of this change, make rendering use `app.snapshot` as its only snapshot source. Removing the
second snapshot parameter closes an existing way for styled rows and hit rows to describe different
data.

### Tier behavior

- Normal exposes all three panes, both handles, the file viewport, and the bottom tabs.
- FocusOnly exposes only the focused pane. If Impact is focused, its tabs remain active. There are
  no resize handles. Hidden panes have `None`, not stale rectangles.
- Explicit zoom and automatic compact fallback use the same visible targets. Only explicit zoom
  gets the existing `ZOOM` title tag (`render.rs:182-189`).
- TooSmall exposes no pane, tab, row, or handle target.
- Modal rectangles may also be computed here from the current `centered` policy
  (`render.rs:1693-1705`), even though this version treats the whole screen as an interaction
  shield while a modal is open.

## Shared files-list row model

A click cannot use `row - files.inner.y` as a logical index. There are three coordinates:

1. logical/selectable index (`App::file_sel`),
2. physical item index (including notes), and
3. visible screen row after scroll.

Create one structural projection and make both rendering and App lookup helpers use its rules:

```rust
pub enum ProjectedFileRow {
    File {
        file_index: usize,
        logical_index: usize,
    },
    Symbol {
        file_index: usize,
        symbol_index: usize,
        logical_index: usize,
    },
    Note {
        file_index: usize,
        kind: SemanticNote,
    },
    Empty,
}

pub struct VisibleFileRow {
    pub physical_index: usize,
    pub rect: Rect,
    pub target: Option<FileHit>,
}

pub struct FileHit {
    pub logical_index: usize,
    pub file_index: usize,
    pub symbol_index: Option<usize>,
}

pub struct FileViewport {
    pub inner: Rect,
    pub first_visible: usize,
    pub all_rows: Vec<ProjectedFileRow>,
    pub visible_rows: Vec<VisibleFileRow>,
}
```

Projection rules must match what is drawn now:

- every file contributes one selectable `File` row;
- an expanded Ready file with symbols contributes selectable `Symbol` rows;
- an expanded Loading, Unsupported, Failed, Unloaded, or Ready-empty file contributes exactly one
  non-selectable `Note` row;
- an empty file list contributes one non-selectable `Empty` row.

This also closes a latent invariant hole. `App::flat_file_rows` currently counts `f.symbols` for
any expanded file (`app.rs:609-617`), while rendering hides symbols for non-Ready semantic states
(`render.rs:510-553`). Dispatcher snapshots normally keep those consistent, but the public types do
not enforce it.

All items are currently one cell high. Let `capacity = files.inner.height as usize` and find the
selected row's **physical** index from the projection. To preserve the current fresh-`ListState`
behavior:

```text
first_visible = min(
    (selected_physical + 1).saturating_sub(capacity),
    all_rows.len().saturating_sub(capacity),
)
```

Use zero when there is no selection or capacity. Assign each visible row the exact one-cell `Rect`
at `inner.y + slot`. Draw the pane block separately, then draw exactly that visible slice in the
inner rectangle. There is no need for `ListState`: active and owner backgrounds are already built
into each row at `render.rs:477-555`. Hit-testing searches the same `VisibleFileRow.rect`s. A Note,
Empty, border, or blank tail has `target: None`.

Prefer indices in the frame projection and resolve them immediately against the same snapshot.
The run-loop invariant above prevents a snapshot update between draw and event dispatch. If that
invariant changes later, add a monotonically increasing frame generation; snapshot epoch alone is
not sufficient because Loading/Ready publishes can change rows within one repo epoch.

## Resizable Impact height

Replace the fixed-only use of `IMPACT_HEIGHT` (`layout.rs:31-32`, `render.rs:83-100`) with:

```rust
pub const DEFAULT_IMPACT_HEIGHT: u16 = 9;
pub const MIN_IMPACT_HEIGHT: u16 = 5;
pub const MAX_IMPACT_HEIGHT: u16 = 18;
pub const MIN_WORK_HEIGHT: u16 = 7;

pub fn impact_height(request: u16, frame_height: u16) -> u16 {
    let available = frame_height.saturating_sub(1 + 1 + MIN_WORK_HEIGHT + 1 + 1);
    request
        .clamp(MIN_IMPACT_HEIGHT, MAX_IMPACT_HEIGHT)
        .min(available)
}
```

Add `pub impact_height: u16` to `App`, initialized to 9 next to `files_width`
(`app.rs:51-58,89-112`). Like `files_width`, it is a requested preference. Geometry may reduce it
to preserve the seven-row work area without rewriting the preference. Keep `choose_tier` at
`80x20`: at the minimum Normal size the effective maximum remains 9, so the existing 7-row work
contract and default geometry remain unchanged. A five-row Impact has three interior rows and is
still useful; 18 prevents the bottom pane from dominating a large terminal while doubling its
default capacity.

## Drag lifecycle

Keep gesture state in `run`/`mouse.rs`, not `App`; only the final/current dimensions are durable
view state.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragBoundary {
    FilesDiff,
    WorkImpact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragState {
    Idle,
    Dragging {
        boundary: DragBoundary,
        start_pointer: Position,
        start_extent: u16, // effective value in the drawn geometry
        start_area: Rect,
        moved: bool,
    },
}
```

Recommended targets in Normal:

- Files|Diff: the two adjacent border columns around `diff.outer.x`, excluding the work
  bottom-border row used by the horizontal boundary. This avoids an ambiguous T-junction.
- Work|Impact: the work bottom-border row plus the Impact top-border row, full width. Bottom tabs
  override it where their label rectangles overlap.

State transitions:

1. `Idle + Down(Left)` on a handle enters `Dragging`. It does not change a size.
2. `Dragging + Drag(Left)` computes a signed delta from the start. Apply the absolute setter live
   and set `moved = true` only when the pointer moved.
3. `Dragging + Up(Left)` computes one final sample from the release coordinate, then returns Idle.
   It does so even outside every pane/handle. A down/up at the same coordinate emits no setter, so
   clicking a border cannot jump the layout.
4. Any button up ends a matching drag. A new unrelated button-down cancels it. Stray Drag/Up while
   Idle is inert.
5. If the terminal area changes but remains Normal, rebase on the first subsequent Drag sample:
   set `start_pointer` to that sample and `start_extent` to the newly rendered effective extent,
   emit no resize for that sample, and continue. This avoids a jump caused solely by resize.
6. If resize, zoom, compact fallback, or a modal removes the active handle, cancel the drag. An Up
   outside the terminal cannot be observed; a later Down must always replace stale drag state, so
   recovery is deterministic.

Use the **effective drawn extent** as the start, not the stored request. At width 80, for example,
`files_width(42, 80)` draws 32 (`layout.rs:108-114`); starting from 42 would make the pointer move
10 cells before the divider responded.

Mapping formulas, before fixed and dynamic clamps:

```text
files_width  = start_extent + (current_x - start_x)
impact_height = start_extent - (current_y - start_y)
```

Thus right widens Files, left narrows it, up enlarges Impact, and down shrinks it. Clamp Files to
`MIN_FILES_WIDTH..=min(MAX_FILES_WIDTH, work.width - MIN_DIFF_WIDTH)`. Clamp Impact to
`MIN_IMPACT_HEIGHT..=min(MAX_IMPACT_HEIGHT, frame.height - 4 - MIN_WORK_HEIGHT)`. Use signed
intermediates and saturating conversion; do not subtract `u16` coordinates directly.

## Mouse routing and Action changes

Add a pure router in a new `crates/codescope-tui/src/mouse.rs`:

```rust
pub struct MouseOutcome {
    pub action: Action,
    pub drag: DragState,
    pub dirty: bool,
}

pub fn map_mouse(
    event: MouseEvent,
    app: &App,
    geometry: &UiGeometry,
    drag: DragState,
) -> MouseOutcome;
```

This is the stateful analogue of `map_key`, but remains a pure transition: input state in, next
state and one intent out. A single atomic action is important because `dispatch` synchronizes
selection at its end (`run.rs:264-266`). Do not implement a row click as `Focus(Files)` followed by
`Down` or by a second selection action; that can report the old target before the clicked one.

Add these `Action` variants in `action.rs`:

```rust
SelectFileRow { logical_index: usize },
SetBottomView(BottomView),
SetFilesWidth(u16),
SetImpactHeight(u16),
```

All four are view-local:

- `SelectFileRow` sets `focused = Pane::Files` and `file_sel = logical_index` atomically. The normal
  `dispatch` tail then calls `SelectionTracker::sync`, which produces the existing dispatcher-owned
  diff retarget and lazy relation fetch. Never emit `SelectionChanged` directly from `map_mouse`.
- `SetBottomView` sets `focused = Pane::Impact`, selects the named tab, and resets
  `ai_plan_scroll` only when the view actually changes. Keep `ToggleBottomView` for `v`.
- The two absolute setters clamp the stored preference to their fixed range. Geometry applies the
  terminal-dependent clamp. They are not sent to the dispatcher.
- `DragState` transitions are not Actions and do not belong in snapshots.

`run.rs` gains an explicit `Event::Mouse(mouse)` arm next to Key/Resize (`run.rs:48-56`), routes it
against `last_geometry`, saves the returned drag state, and sends the returned Action through the
existing dispatch path. Row selection therefore reuses `SelectionTracker` exactly.

Crossterm's `EnableMouseCapture` enables all-motion reporting. With the current biased select and
unconditional draw at the top of every loop (`run.rs:39-48`), a stream of ignored `Moved` events
can starve snapshots/ticks and draw without limit. As part of the event-loop change:

- remove `biased`, or explicitly budget mouse events;
- keep a `dirty` flag so Moved/no-op events do not force a draw;
- return or disable the arm on EventStream EOF/error instead of leaving an always-ready source;
- return or disable the snapshot arm after the watch sender closes instead of repeatedly hitting
  `continue` (`run.rs:60-64`).

The defensive raw-mode call in `run.rs:31-33` should also go away. Terminal mode and mouse capture
should have one owner in `codescope/src/terminal.rs`, and ignored initialization errors are unsafe.

## Modal and zoom behavior

- If any of `show_help`, `show_model_picker`, or `show_base_picker` is true
  (`app.rs:71-84`), consume every mouse kind and cancel drag. This version does not add mouse
  controls inside modals. Clicking outside does not close a modal and never reaches a pane.
- The three independent booleans already have an ambiguity: render z-order is Help, Model, Base
  (`render.rs:137-145`), while key routing checks Help, Model, Base and stops at the first true
  (`action.rs:137-152`). A future cleanup should replace them with one `Modal` enum. It is not
  necessary for this mouse slice if `modal_open()` blocks the whole screen.
- Zoom and automatic FocusOnly contain only the focused pane (`render.rs:104-154`). Do not retain
  hidden-pane hits from a previous Normal frame. There are no handles. Impact tabs work only when
  Impact is the visible pane.

## Wheel decision

Defer wheel input in this change and return no action for `ScrollUp`, `ScrollDown`, `ScrollLeft`,
and `ScrollRight`.

The diff and AI plan have explicit scroll offsets, but Files does not. Its `ListState` offset is
created and discarded each render. Reusing `Up`/`Down` for a wheel would change selection, send
`SelectionChanged` per notch, retarget the diff, and potentially start lazy relation work. That is
not normal list scrolling. A follow-up should add a persistent `files_scroll`/viewport offset and a
targeted `ScrollPane { pane, delta }` that scrolls the pane under the pointer without stealing
keyboard focus. Horizontal wheel should affect only an unwrapped Diff. Deterministic Impact remains
inert.

## Terminal capture lifecycle

The minimal safe ownership change belongs in `crates/codescope/src/terminal.rs`, around the current
`run_with_terminal` (`terminal.rs:12-20`):

1. Call `ratatui::init()` as today.
2. Immediately wrap ratatui's newly installed panic hook. The wrapper best-effort executes
   `DisableMouseCapture`, then calls the captured ratatui hook, which restores raw mode and leaves
   the alternate screen.
3. Create an armed `TerminalSessionGuard` **before** enabling capture. Its `Drop` independently
   attempts `DisableMouseCapture` and then calls `ratatui::restore()`. Never use `?` between those
   cleanup steps; a disable failure must not skip restore.
4. Execute `EnableMouseCapture` on stdout and propagate an entry error. Because the guard is already
   armed, even a partially written enable sequence is followed by disable+restore.
5. Await the supplied future. Let the guard clean up before returning its original result.

Sketch:

```rust
let terminal = ratatui::init();
install_mouse_cleanup_panic_hook(); // chains the ratatui hook
let cleanup = TerminalSessionGuard::armed();
execute!(std::io::stdout(), EnableMouseCapture)?;
let result = f(terminal).await;
drop(cleanup); // DisableMouseCapture, then ratatui::restore()
result
```

The guard covers `Ok`, returned `Err`, unwind, and dropping/aborting the async future. The panic hook
also covers panic-abort configurations where destructors do not run. Double disable/restore during
an unwinding panic is harmless and preferable to leaving capture active. No Rust cleanup can cover
`SIGKILL`, `std::process::abort`, or power loss; “cancellation” here means dropping/aborting the Rust
future.

The existing PTY panic injection initializes ratatui directly in `main.rs:65-70`; it must invoke a
panic inside `run_with_terminal`, or a new hook must do so. The PTY assertion currently checks only
alternate-screen enter/leave (`crates/codescope/tests/terminal_restore.rs:58-67`), so it cannot prove
mouse cleanup.

## Help and footer

Update the wide footer groups at `render.rs:1599-1647` to fit within 96 cells, for example:

```text
Tab analyze · click/1-3 pane · z zoom · W wrap · n/N hunk · drag/[/] resize · v view · ? help
```

Update the modal heading at `render.rs:1651-1688` from “keyboard controls” to “controls” and add:

```text
mouse click       focus pane / select file or symbol / choose bottom tab
mouse drag border resize files and Impact panes
```

Do not mention wheel yet. Also remove or correct the stale text “Tab still switches” on the zoom
line (`render.rs:1673`): current Tab expands/analyzes a file and is inert outside Files
(`action.rs:168-174`); it does not switch panes.

## Module/type change list

1. **`layout.rs`** — keep tier/files helpers; add Impact height constants/helper and
   `MIN_WORK_HEIGHT`.
2. **new `file_rows.rs`** — authoritative physical/logical projection, target lookup, note kind,
   and shared resolver used by App, geometry, and rendering.
3. **new `geometry.rs`** — `UiGeometry`, pane/inner rectangles, viewport, tabs, handles, modal
   rectangles, and all Normal/FocusOnly/TooSmall splits.
4. **`render.rs`** — accept `&UiGeometry`, use `app.snapshot` only, render its pane rectangles and
   exact visible file slice, and build tab title/hits from one label definition.
5. **`app.rs`** — add `impact_height`; apply four new local Actions; delegate file flatten/resolve
   helpers to the shared projection.
6. **`action.rs`** — add the four idempotent/absolute Actions; keep `map_key` unchanged.
7. **new `mouse.rs`** — pure hit routing plus drag state machine.
8. **`run.rs`** — retain the last drawn geometry, handle `Event::Mouse`, remove raw-mode ownership,
   prevent all-motion starvation, and preserve one selection sync per click.
9. **`terminal.rs`** — capture enable, panic-hook chaining, and cancellation-safe RAII cleanup.
10. **`main.rs` / `terminal_restore.rs`** — route the panic test through the real session and assert
    mouse enable/disable sequences.
11. **`lib.rs`** — export only the geometry/mouse API needed by tests or the binary; keep row-model
    internals crate-private where possible.

## Test plan — 24 concrete tests

Use `TestBackend` sizes for frame plans and a helper that synthesizes real crossterm values:

```rust
fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE }
}
```

1. `geometry_normal_140x40_exact`: default pane/chrome rectangles, both two-cell handles, and
   Impact height 9.
2. `geometry_minimum_80x20_exact`: Files 32, Diff 48, work 7, Impact 9, with dynamically clamped
   setters.
3. `impact_height_clamps_and_preserves_work`: table over requests 0/5/9/18/max and heights
   20/21/40; work never drops below 7.
4. `geometry_focus_only_and_zoom_targets`: table over all focused panes and compact/explicit zoom;
   only the visible pane is hittable and handles are absent.
5. `geometry_too_small_has_no_targets`: `29x8` and `30x7`; panes, tabs, rows, and handles are None.
6. `file_projection_ready_expansion`: collapsed file, expanded file, multiple symbols; physical and
   logical indices and `(file, symbol)` targets are exact.
7. `file_projection_notes_and_empty_are_not_selectable`: table over Loading, Unsupported, Failed,
   Unloaded, Ready-empty, and no files; each consumes one physical row and no logical index.
8. `file_viewport_scrolls_by_physical_rows`: a note before a selected later symbol; assert
   `first_visible`, every screen y, and the clicked logical target at a deterministic small height.
9. `bottom_tab_rects_match_rendered_labels`: Normal and Impact zoom, with AI Loading suffix; every
   label cell hits its view and separator/suffix cells do not.
10. `file_borders_blank_tail_and_note_do_not_select`: deterministic border, empty interior tail,
    and note coordinates focus Files but preserve `file_sel`.
11. `left_click_focuses_each_visible_pane`: synthesized Down(Left) in Files, Diff, and Impact equals
    the corresponding focus behavior.
12. `click_file_and_symbol_syncs_once`: each synthesized row click atomically focuses/selects and
    causes exactly one final `SelectionChanged`; symbol includes `(name,line,col)`, repeat click
    sends none.
13. `click_note_focuses_without_selection_sync`: note click changes focus only and does not retarget
    the dispatcher.
14. `tab_click_sets_named_view_idempotently`: both labels focus Impact, set the exact view, reset AI
    scroll only on a change, and never toggle on repeat.
15. `tab_label_precedes_horizontal_drag_handle`: a coordinate in the overlapping top border selects
    a tab; separator at the same border starts horizontal drag.
16. `modal_swallows_mouse_and_cancels_drag`: help/model/base table; row, tab, handle, wheel, and
    outside clicks produce no underlying action and next state is Idle.
17. `vertical_divider_click_without_motion_is_noop`: Down/Up at either border column leaves the
    stored preference unchanged.
18. `vertical_drag_uses_effective_width_clamps_and_releases_outside`: start at width-80 effective
    32, move both directions, hit min/dynamic max, and end outside the work row.
19. `horizontal_drag_direction_clamps_and_releases_outside`: upward enlarges, downward shrinks,
    fixed/dynamic clamps hold, and a final Up coordinate is committed outside the pane.
20. `drag_rebases_after_resize_and_cancels_when_handle_disappears`: resize within Normal causes a
    zero-change rebase; resize to FocusOnly or opening zoom/modal cancels.
21. `ignored_mouse_kinds_are_clean_noops`: Moved, right/middle Down, stray Drag/Up, and all four
    wheel kinds return no Action and `dirty == false` under the deferred-wheel policy.
22. `terminal_guard_success_error_and_enable_failure`: fake/injectable terminal ops table proves
    enable/disable/restore ordering; partial enable still disables, cleanup failure still restores,
    and the original closure error is preserved.
23. `terminal_capture_disabled_on_panic_pty`: extend the existing PTY test to assert alternate-screen
    enter/leave plus mouse enable (`?1000h`, `?1006h`) and reverse disable (`?1006l`, `?1000l`) on
    the real panic path.
24. `terminal_guard_cleans_up_when_future_is_dropped`: poll a pending session future until capture
    is enabled, drop/abort it, and assert disable then restore exactly once (idempotent fallback is
    also safe).

No Cargo commands were run for this review.

## Risks and current-spec conflicts

1. **Geometry drift is the primary correctness risk.** Renderer-private Layouts and a second
   hit-test Layout will disagree at `80x20`, the height-12 help-row transition, focus-only, or after
   either resize. Retaining the exact frame plan is non-negotiable.
2. **Physical versus logical file rows can select the wrong entity.** Notes and the discarded
   `ListState` make arithmetic y mapping invalid. The existing expanded/non-Ready symbol count is a
   latent mismatch that the shared projection should remove.
3. **Two snapshot arguments can split pixels from hits.** Use only `app.snapshot` for geometry,
   rendering, and immediate click resolution.
4. **All-motion capture can starve the loop.** Crossterm reports Moved events; the current biased,
   unconditional-redraw loop needs fairness/dirty handling before capture is enabled.
5. **Terminal cleanup after await is not cancellation-safe.** A post-await Disable command alone
   is insufficient. Ratatui's hook restores only the state ratatui enabled, not mouse capture.
6. **The existing panic proof bypasses the lifecycle being changed.** It must run through
   `run_with_terminal` before its mouse assertions are meaningful.
7. **A stored preference may differ from its effective rectangle.** Drag baselines must use the
   effective drawn width/height. Only real pointer movement should overwrite a constrained stored
   preference.
8. **Modal state is not exclusive.** The current three booleans can disagree on visual versus input
   precedence. Full-screen swallowing is safe now; one `Modal` enum is the long-term fix.
9. **Selection dispatch is awaited and records before send success** (`run.rs:133-139`). Wheel is
   deferred, so mouse does not add high-rate semantic selection traffic. A later wheel feature
   should use a latest-wins/pending tracker rather than await every notch.
10. **No cleanup can handle hard process termination.** Document that the guarantee covers normal
    return, returned errors, Rust panic hooks/unwind, and future cancellation—not `SIGKILL` or
    `std::process::abort`.
