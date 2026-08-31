# Review 13 — Tall/narrow layout redesign

## Decision

Use width for **minimum readable pane widths**, and height for whether a vertical stack can pay
for three bordered panes. Do not call every terminal at 120 columns “wide.” The current
`30 + Min(40) + 36` split is legal to Ratatui at 120 columns, but legal is not readable: after
borders and the diff gutter, it leaves roughly 24 cells for a file path, 45 for source text, and
34 for a semantic row.

The default for a tall viewport below 150 columns should be a vertical master/detail/context
stack:

```text
┌ changed (18) · @ sandbox/vm-sandboxes/packages/ ┐  10 rows
│ …/control-plane/…/executor.go                    │
└──────────────────────────────────────────────────┘
┌ executor.go · hunk 2/5 ─────────────────────────┐  all surplus rows
│   142 +let sandbox = build_sandbox(              │
│       ↪     package, runtime, executor, ...);    │
└──────────────────────────────────────────────────┘
┌ callers of build_sandbox ───────────────────────┐  10 rows
│ build_vm  calls                                  │
└──────────────────────────────────────────────────┘
```

This spends height, which the problem viewport has, to recover width for all three concerns. It
also preserves simultaneous context. A popup or Tab is then an escape hatch, not the normal way
to discover relationships.

## Exact layout tiers

Keep the existing too-small boundary. Compute the outer chrome first. At height 12 or more use:

```rust
Layout::vertical([
    Constraint::Length(1), // top state
    Constraint::Min(3),    // main
    Constraint::Length(1), // contextual keys/message
])
```

At heights 8–11, drop the footer first and use `[Length(1), Min(3)]`. At `width < 30` or
`height < 8`, render only `terminal too small (WxH)` as today. The footer is the first vertical
luxury to go; pane content and repository state remain.

After that, choose the first matching geometry below. The sizes include each pane's borders.

| Tier | Condition | Ratatui constraints | Result |
|---|---|---|---|
| Focus zoom | `app.zoomed` | no split; focused pane gets `main` | Deliberate full-main inspection, at every usable size. |
| Spacious | `width >= 150` | horizontal `[Length(38), Min(72), Length(40)]` | Files, diff, and relations in columns. |
| Tall/narrow | `48 <= width < 150 && main.height >= 34` | vertical `[Length(10), Min(14), Length(10)]` | All three panes at full terminal width; the diff absorbs every extra row. |
| Medium/shallow | not above and `width >= 80` | horizontal `[Length(32), Min(48)]` | Files plus one detail slot. The slot shows diff normally and relations when `Pane::Semantic` is focused. |
| Focus-only | all other usable sizes | no split; focused pane gets `main` | `Tab`/`BackTab` or `1`/`2`/`3` changes the visible pane. |

The tall tier therefore starts at a total height of 36 when both one-line bars are present. Its
minimum interiors are 8 file rows, 12 diff rows, and 8 relation rows. At its minimum width of 48,
the pane interior is 46 cells and a numbered diff line has 39 cells of source text. Those are
small but useful. Below either minimum, paying six rows for three independent `Block` borders is
not worthwhile.

For the shallow medium tier, replace the current centered 60%-by-70% semantic popup with the
second-slot swap. Render `Clear` plus `render_semantic` into `panes[1]`, not a percentage of all of
`main`. This leaves the file navigator visible and gives relations the same `Min(48)` width and
full height as the diff. It also avoids a modal-looking surface for ordinary pane focus.

These thresholds deliberately avoid an aspect-ratio calculation. `Rect` reports character cells,
not physical pixels; font cell aspect ratios vary. Absolute pane minima describe readability more
reliably. The height gate is the useful definition of “tall” here.

### Why 150 columns for three columns

At 150, the spacious tier guarantees these interior budgets:

- files: 36 cells, of which 32 remain after `M ▾ `;
- diff: at least 70 cells, of which 63 remain after the six-cell line-number gutter and sign;
- relations: 38 cells.

This is still compact, so wrapping and elision remain useful, but none of the panes is merely a
sliver. At larger widths, the fixed side panes stop growing and `Min(72)` gives all surplus to the
diff. A percentage split would waste source width on already-readable navigation lists.

## Focus zoom

Add `App::zoomed: bool` and `Action::ToggleZoom`. Bind it to **`z`**. It is currently unused,
widely understood as “zoom” in TUIs, and does not collide with the scope or AI keys.

- `z` renders the focused pane into all of `main`; the compact top bar and footer stay visible.
- `Tab`, `BackTab`, and `1`/`2`/`3` continue to change focus while zoomed, so zoom also becomes a
  fast three-view switcher.
- A second `z` exits. `Esc` may also exit zoom when no modal is open, but is not the only exit.
- Resize does not clear zoom. In the automatic focus-only tier, zoom changes no geometry, but the
  state remains meaningful if the terminal later grows.
- Help/model/base pickers still render last and therefore cover the zoomed pane normally.

Show `ZOOM` or `z` in the focused pane title while active. Otherwise, at a narrow size the user
cannot tell automatic focus-only from a pinned zoom.

Zoom is not a replacement for the vertical stack. It is for reading a dense relation tree or an
alignment-sensitive diff after the overview has answered “what changed, what does it look like,
and what does it affect?”

## Paths: shared root, then uniqueness-preserving middle elision

Keep the full repo-relative path in `UiSnapshot::files`; display shortening must never become the
identity used by selection or `SelectSymbol`. Derive display paths in the renderer (or cache the
derivation when a snapshot arrives).

### 1. Extract a shared root

1. Split paths on `/` and compare **directory components**, excluding each basename. Do not use a
   raw string prefix: `packages/api` and `packages/api-old` do not share the `api` component.
2. Use a shared root only when there are at least two files, at least two shared directory
   components, and removing it saves at least eight display cells. This avoids turning a useful
   short `src/` or `packages/` into extra chrome.
3. Put the root once in the pane title:
   `changed (18) · @ sandbox/vm-sandboxes/packages/`. Elide that title root with the same
   component algorithm if needed.
4. File rows are relative to `@`, but retain a leading `…/` to make the omission explicit. Thus a
   second middle omission can read `…/control-plane/…/executor.go`. The snapshot and actions
   retain the original full path.

For files under one deep monorepo package, this removes dozens of repeated cells without hiding
where the list is rooted.

### 2. Preserve the distinguishing parts of each row

For the current renderer, the exact row path budget is:

```text
budget = pane.width - 2 border cells - width("M ▾ ")
       = pane.width - 6
```

Use terminal display-cell widths, not `str::len()` and never byte slicing. A small helper should
iterate Unicode grapheme clusters and measure them with `unicode-width`; paths can contain CJK,
combining marks, or emoji even if most repositories do not.

For every path remainder (after an optional shared-root strip):

1. Prefix the candidate with `…/` when a shared root was stripped. If that candidate fits, render
   it without any further shortening.
2. Compute the shortest suffix of whole path components that is unique among the changed files.
   This means two `executor.go` files naturally retain `worker/executor.go` and
   `control-plane/executor.go`.
3. Preserve the basename/unique suffix first. If space remains, preserve the first divergent
   component, insert a middle `…/`, and greedily add complete components immediately before the
   unique suffix from right to left. For example, after root stripping,
   `control-plane/generated/internal/executor.go` becomes
   `…/control-plane/…/executor.go`.
4. If `first/…/unique-suffix` does not fit, drop the first component and use
   `…/unique-suffix` (one ellipsis can represent both omissions). If even the basename does not
   fit, middle-elide that single component, retaining its extension:
   `extraordinar…ecutor.go`.
5. If an extreme width makes two final strings collide, reserve three cells for a stable ordinal
   (`·01`, `·02`) assigned by sorted full path. This is a last-resort disambiguator, not the normal
   display.

The same helper should elide `DiffPane.title`, but reserve the right side first for
`hunk 2/5`; state indicators must not disappear because a path happened to be long. When a file
row is selected, the bottom message area can show the full path instead of the generic key hints.

## Diff: smart wrap by default, raw horizontal mode on demand

For this viewport, exchanging surplus vertical cells for scarce horizontal cells is the right
default. Restore wrapping, but do **not** solve it by adding only
`Paragraph::wrap(Wrap { trim: false })` to the current widget. Ratatui then wraps spans at column
zero, continuation lines lose the diff gutter, and `Paragraph::scroll` counts visual rows while
`App::diff_scroll` is clamped to logical `DiffRow`s. That is how `G`, page movement, and hunk jumps
end up short of the real bottom.

Pre-wrap each `DiffRow` into display `Line`s for the current inner width:

- Keep the first-line prefix as the current six-cell number gutter plus `+`, `-`, or a space.
- Give every continuation a seven-cell hanging prefix, `"      ↪"`, so code starts in the same
  column and no continuation can be mistaken for a new diff line.
- Preserve all source whitespace. Prefer a whitespace or punctuation break near the edge, but do
  not trim it; hard-break an overlong token by grapheme/display width. Expand tabs consistently to
  four-cell tab stops for measurement and display.
- Build `first_visual_line[logical_row]` while wrapping. Render with
  `scroll.y = first_visual_line[app.diff_scroll]`. Thus the existing logical-row anchor survives a
  resize and `G` can map the last logical row to its real wrapped position.
- Hunk navigation should set the logical anchor to the hunk header row, then use the same map.

Add `App::diff_wrap`, default `true`, and bind **`W`** to toggle it while the diff is focused.
Uppercase `W` is free; lowercase `w` remains Working scope. In raw mode, keep clipping plus the
existing `h`/`l` or arrow horizontal scroll in eight-cell steps, and bind **`0`** to reset x.
Do not apply Ratatui's x-scroll to the whole `Line`, because that scrolls the line number and sign
offscreen too. Keep the seven-cell gutter fixed and display-cell-slice only the source body; clamp
x to the longest body minus the body viewport and reset it when the selected file/scope changes.
In wrapped mode, horizontal offset is zero and `h`/`l` should not silently move hidden state. Show
`wrap` or `x+NN` in the diff title so the mode is explicit.

Raw mode matters for tables, ASCII art, and indentation comparisons. It should be available, but
it should not force every tall/narrow user to pan every ordinary code line.

## Compact chrome and degradation order

The current top bar puts the least compressible data first and lets Ratatui clip whatever happens
to land on the right. Build left and right groups to a measured budget instead.

| Width | Top bar |
|---|---|
| `>= 150` | Full labels: product, repo, `branch ◂ base`, divergence, scope, LSP, AI, and model when it fits. |
| `80..149` | Left: elided `repo  branch ◂ base`. Right-reserved: `[B/S/U/W] L✓ A✓ ⟳`. Drop product, divergence, and model. |
| `50..79` | Left: elided repo. Right-reserved: compact scope, LSP, AI, refresh. Drop branch/base. |
| `30..49` | Elided repo plus compact scope; retain refresh or failure glyphs, omit healthy service glyphs. |

Implement each row as a horizontal split
`[Constraint::Min(1), Constraint::Length(measured_right_group)]`; this guarantees that a long
branch cannot clip health or refresh state. Compact status glyphs need a legend in help (`L~`
degraded, `L!` failed, and equivalent AI states). Failure always outranks a healthy glyph or a
model name.

The footer should be contextual and shorter than the current permanent key catalog:

- width `>= 100`: full hints, including `z zoom` and `W wrap`;
- width `60..99`: `Tab pane · z zoom · ? keys · q quit`;
- width `30..59`: `Tab · z · ? · q`;
- height `8..11`: no footer.

If `UiSnapshot.message` is non-empty, render the message first and add hints only if they fit; do
not concatenate a long message with a long fixed help string and rely on clipping.

The overall degradation order is therefore:

1. remove model name, ahead/behind counts, branding, and verbose key hints;
2. elide repeated path components and wrap diff content;
3. when height can afford it, change columns to the three-pane vertical stack—drop no concern;
4. when height cannot afford the stack, drop simultaneous relations first (it remains one Tab
   away in the medium detail slot);
5. below 80 columns, show only the focused concern unless the tall stack's 48-column minimum is
   met;
6. below 30×8, stop pretending the interface is usable and show the size message.

## Ratatui implementation and acceptance checks

This design uses only normal `Layout`, `Rect`, `Clear`, `List`, and `Paragraph` behavior. The two
parts Ratatui does not provide are width-aware path elision and a gutter-aware wrapped diff; both
should be pure helpers and snapshot-testable. Ratatui also does not merge adjacent `Block` borders,
so the vertical tier intentionally budgets six border rows. A later shared-border widget could
save two rows, but it is not needed for this change.

Add headless render tests at the boundaries and with realistic long content:

- `149×60`: three full-width stacked pane titles are all present;
- `150×36`: three columns meet 38/72/40 and all titles are present;
- `100×36`: stack; `100×35`: medium files/detail;
- `48×36`: stack; `47×60`: focus-only;
- `79×30`: focus-only; `80×30`: files/detail;
- `30×8`, `29×8`, and `30×7`: usable boundary and both too-small boundaries;
- two long same-basename paths remain distinguishable;
- a Unicode path never splits a grapheme or exceeds its cell budget;
- a long diff line shows a hanging continuation, and `G`/hunk navigation reaches the final
  logical row in both wrap and raw modes;
- `z`, Tab-while-zoomed, resize-while-zoomed, and `W` mode persistence are deterministic.

The important screenshot assertion is not merely “does not panic.” At `120×50`, it must contain
all three pane titles, an unambiguous `executor.go` row, wrapped code rather than a clipped right
edge, and a readable semantic label.
