# Review 25 — diff tab rendering design

Read-only diagnosis and implementation design for HEAD
`080380c451cbd3516c24a12d4a0ed7d378363778`. I did not change Rust source and did not run a
Cargo command. This document is the only repository file I wrote.

## Recommendation

Fix this at the final display boundary in `crates/codescope-tui/src/render.rs`. Keep every
`DiffLine.text` and `DiffRow.text` byte-for-byte unchanged. Compute intraline flags from the
original graphemes and original byte spans, then replace each tab with styled literal spaces while
materializing Ratatui spans.

Use one constant and one width rule:

```rust
const TAB_STOP: usize = 4;

fn tab_cells(col: usize) -> usize {
    TAB_STOP - (col % TAB_STOP)
}
```

A raw viewport that starts or ends inside a tab must paint the tab cells that intersect the
viewport as spaces. Dropping the whole tab is not acceptable because it moves all following source
text left. Wrapped segments must expand with the same segment-local display column used to choose
their ranges. The line-number/sign gutter is assembled separately and never contributes to that
column.

## Confirmed data and render path

### Source text is already authoritative upstream

No upstream normalization is needed or wanted.

- `crates/codescope-git/src/diff.rs:188-214` removes exactly the first ASCII unified-diff marker
  and passes `&line[1..]` to `DiffLine::context`, `DiffLine::del`, or `DiffLine::add`. A source tab
  after that marker remains the first byte of `DiffLine.text`.
- `crates/codescope-core/src/git.rs:368-414` owns that text as a `String` without transforming it.
- `crates/codescope/src/dispatcher.rs:1456-1497` clones `l.text` directly into `DiffRow`.
- `crates/codescope-tui/src/snapshot.rs:183-228` transports each row's text as an owned `String`.
- `crates/codescope-tui/src/intraline.rs:83-121,207-220` returns byte ranges into the original
  old/new strings. It does not return offsets into display-expanded text.

The external reproduction is also ordinary source indentation. At the time of this review,
`platform-2/.../sandbox_runtime.go:318-320` contains one tab, two tabs, and one tab respectively.
The live identifier is currently `f.rootfsd`; the requested synthetic `f.rootfs` fixture has the
same geometry.

### The mismatch is in final materialization

`render_diff` computes a source-body width separate from the gutter at
`crates/codescope-tui/src/render.rs:778-803`. Both layout paths already charge tabs mathematically:

- raw maximum width and `effective_x` use `measured_cells` at `:984-992`;
- wrapping uses `grapheme_cells` through `wrap_ranges` at `:1248-1269`;
- `grapheme_cells` currently charges `4 - (col % 4)` at `:1214-1221`.

The final strings disagree with that math:

- `styled_graphemes` appends the original grapheme to a `Span` at `:1154-1163`;
- `slice_styled` does the same at `:1171-1201`;
- `wrap_body` concatenates original graphemes at `:1238-1243`.

For Ratatui 0.30.2, `ratatui-core`'s `Buffer::set_stringn` filters any grapheme containing a control
character before it assigns cells (`ratatui-core-0.1.2/src/buffer/buffer.rs:350-353`). A literal
`	` therefore paints zero cells. The renderer has charged space that Ratatui never receives. The
comment at `render.rs:1233-1237` claiming Ratatui and terminals render the tab is reversed.

This was already required by `docs/review/15-redesign-spec.md:355-359` and recorded as an unresolved
MAJOR in `docs/review/16-redesign-review.md:64-70`.

## Presentation-boundary design

### Shared helpers

Keep `grapheme_cells` as the canonical display-width function, but make it call `tab_cells` and use
`TAB_STOP` rather than a second literal `4`. `measured_cells` remains a fold over the original
graphemes and this function.

Add two small materialization helpers in `render.rs`:

1. An `append_styled_text` helper that appends owned text to `Vec<Span<'static>>`, skips an empty
   append, and extends the last span when its style is equal. This retains the current coalescing
   behavior instead of creating one span per expanded cell.
2. An `append_display_grapheme` (or equivalent) helper that takes `(g, body_col, style)`. For `\t`
   it appends `tab_cells(body_col)` literal spaces with `style`; for another printable grapheme it
   appends the original grapheme. It returns/uses the same width as `grapheme_cells`.

For robustness, other control-containing graphemes can be assigned width zero and omitted so the
helper mirrors Ratatui's filter. Tabs remain the one intentional special case. That is not required
to solve normal Git source, but it prevents another measurement/output mismatch for embedded NUL or
other controls.

The required ordering is:

```text
original DiffRow.text
  -> original Unicode graphemes
  -> changed_flags(original grapheme byte extents, original intraline byte spans)
  -> grapheme_cells at the current source-body display column
  -> literal spaces for a tab, carrying that original grapheme's selected style
  -> Ratatui Span / buffer cells
```

Do not create an expanded `DiffRow`, do not feed expanded text to `row_spans`, and do not change the
Git parser, core model, dispatcher, snapshot, or AI inputs.

### Wrapped source rows

Change `styled_graphemes` to track `col`, initially zero for the supplied visual segment. Select
`base` or `hi` from the existing flag first. Then call the display append helper and advance `col`
by `grapheme_cells(g, col)`. A changed tab consequently becomes a run of highlighted spaces; an
unchanged leading tab becomes a run of base-styled spaces.

`push_numbered` must continue this sequence:

1. segment the **original** `text` into graphemes;
2. call `changed_flags` with the **original** byte spans;
3. call `wrap_ranges` on those original graphemes;
4. materialize each selected range through the changed `styled_graphemes`.

`wrap_ranges` resets its display column when it selects a new range, so materialization must also
start at body column zero for each range. Do not expand a whole logical line once and slice that
string afterward. For example, with budget 6, `abcdef\tX` wraps before the tab. The continuation
range measures that tab at its own column zero (four spaces), not at logical column six (two
spaces). This keeps range selection and emitted cells identical.

The first visual line keeps the numbered gutter and source sign. Every later visual line keeps the
existing blank dual gutter and `↪`. Those gutter spans are appended outside the source materializer,
so a leading tab starts at source-body column zero on both kinds of line. Do not copy the logical
line's leading indentation onto a continuation. Only an actual tab present at that continuation
range expands there.

Change `wrap_body` as well: map each original `(start, end)` range through a plain version of the
same segment-local tab expander rather than `concat()`. This covers wrapped hunk bands and removes
the false comment. In raw hunk mode, expand a header from header/band column zero before calling the
generic truncation helper; otherwise a tab in the `@@ ... @@` section still leaks through a
zero-width `truncate_cells` path. Hunk headers have no numbered source gutter, so their column zero
is the band start.

### Raw source rows and horizontal scrolling

Keep `build_raw`'s width/clamp calculation. `max_body`, `effective_x`, and body clipping already use
source-body cells when `measured_cells` is canonical. Replace `slice_styled`'s current
"drop a straddling grapheme, then take" logic with an absolute cell-window intersection.

For window `[skip, end)`, where `end = skip.saturating_add(budget)`, and each original grapheme cell
extent `[col, next)`:

```text
if next <= skip:                    skip it and advance source col
if col >= end:                      stop
visible = [max(col, skip), min(next, end))
if g is a tab:                      append visible.len styled spaces
if an ordinary g is wholly visible: append g with its style
if a wide g intersects only an edge: append visible.len styled spaces (or equivalent padding)
advance source col to next
```

The padding for an edge-clipped wide grapheme fixes the already documented absolute-window issue
from review 16 as a small consequence of using one correct slicer. It keeps later content at its
reported cell even though half of an indivisible glyph cannot be drawn. At minimum, tabs must use
the intersection rule at both edges.

Examples for a five-cell viewport over `ab\tXabcdefgh` are:

```text
skip 0: "ab  X"
skip 2: "  Xab"   # starts exactly at the tab
skip 3: " Xabc"   # starts inside the tab; its visible remainder is retained
```

For `\tX`, `skip=2`, `budget=3` must paint `"  X"`. For `skip=0`, `budget=3`, it must paint three
styled spaces and not paint `X`; this is the right-edge case the current `taken + w > budget`
branch drops completely.

Tab width is always computed from the unscrolled source-body `col`, never from `taken`, screen x,
or the gutter width. Although the UI increments requested horizontal scroll by eight cells, the
longest-line clamp can produce any `effective_x`, including a point inside a tab.

### Intraline alignment and styling

`changed_flags` walks original grapheme byte lengths at `render.rs:1137-1149`. Leave it before all
expansion. A leading tab adds one original byte and one flag, regardless of whether it later paints
one, two, three, or four cells. Thus a changed word after a tab still receives the flag derived from
its original byte range but appears after the tab's four displayed cells.

If an original span overlaps any part of a grapheme cluster, the existing rule highlights the whole
cluster. This remains correct for a combining cluster and for a tab. When a tab's flag is `true`,
every visible expanded cell, including a partially visible raw slice, gets `hi`; otherwise every
cell gets `base`. The style therefore retains add/delete backgrounds and the intraline foreground,
background, bold modifier. Adjacent output with the same style is merged by `append_styled_text`.
The gutter remains a separate group even where its sign happens to have the same style.

### Unicode and coordinate systems

Non-tab graphemes continue to use `UnicodeWidthStr::width`. Because `col` counts display cells:

- `a\tX`, `abc\tX`, and `abcd\tX` place `X` at body columns 4, 4, and 8;
- `界\tX` treats `界` as two cells and places `X` at body column 4;
- `e\u{301}\tX` treats the base-plus-combining-mark grapheme as one cell and places `X` at column 4;
- bytes and scalar counts never enter tab-stop arithmetic.

`gutter_spans` is created before the body is appended, but neither `grapheme_cells`,
`measured_cells`, `wrap_ranges`, `styled_graphemes`, nor `slice_styled` receives its screen column.
This is the correct separation. With `ln_width = 4`, for example, the 13-cell dual gutter/sign ends
immediately before body column zero; a one-tab row starts its first source glyph four cells after
that point regardless of the pane's x coordinate.

## Narrow wrapping and an explicit current-code conflict

`wrap_ranges` promises segments of at most `budget`, but its overflow branch requires
`i > seg_start`. A first grapheme wider than the budget is therefore admitted. A leading tab is four
cells, so direct budgets 1, 2, or 3 currently produce a nominally over-budget range. The `(usize,
usize)` range type cannot express half of the tab grapheme.

Production geometry masks this today: the minimum usable 30-column focused diff has an 11-cell or
larger body even with the six-digit gutter, so a four-cell tab fits an empty segment. The required
very-narrow tests still expose the helper contract, and future layouts should not depend on that
accident.

To satisfy both safe termination and "no segment exceeds the body budget," keep original ranges for
normal wrapping but let presentation materialization split an intrinsically over-budget tab's
literal space run into non-empty chunks of at most `body_w`. Flatten those chunks in
`push_numbered`; only the first chunk of the logical row gets its numbers/sign and every later chunk
gets `↪`. This does not duplicate or remeasure the tab: the one original flag is copied to all four
space cells, and chunking only partitions the already determined four-cell run. Apply the analogous
plain splitting in `wrap_body` if header tabs are covered by the invariant.

A double-width Unicode grapheme in a one-cell body is inherently indivisible: faithful glyph output
and a strict one-cell maximum cannot both hold. The safe fallback is to consume it and paint one
styled padding cell (rather than loop, emit an empty segment, or overflow); reachable Codescope
bodies never take this fallback. Document this fallback if the helper accepts arbitrary positive
budgets.

## Minimal implementation surface

### Production code

Only `crates/codescope-tui/src/render.rs` needs production changes.

- Add `TAB_STOP`, `tab_cells`, a same-style span append helper, and styled/plain display-grapheme
  materializers.
- Change `grapheme_cells` to use the shared constant/helper. `measured_cells` keeps its current fold.
- Change `styled_graphemes` to expand tabs with segment-local columns.
- Change `slice_styled` to use absolute viewport intersection and partial tab spaces.
- Change `wrap_body` to return expanded presentation strings, and correct its comment.
- Expand raw hunk-header presentation before truncation.
- If arbitrary tiny budgets remain part of the helper contract, let `push_numbered` flatten split
  display chunks as described above. Otherwise the "no segment exceeds budget" requirement remains
  unmet even though all reachable panes are safe.

`build_wrapped`, `build_raw`, `raw_numbered`, `changed_flags`, and the normal `wrap_ranges` break
policy need no conceptual rewrite. They should keep original strings/grapheme indices. Their calls
and comments may change to use the shared helpers/chunk result.

No production change belongs in `codescope-git`, `codescope-core`, `dispatcher`, `snapshot`, or
`intraline`.

### Test-only changes

- Add parser preservation coverage in `crates/codescope-git/src/diff.rs`.
- Add `selected_diff` transport coverage in `crates/codescope/src/dispatcher.rs`.
- Put detailed private-helper, built-span, TestBackend cell, style, wrap, and slice tests in the
  `crates/codescope-tui/src/render.rs` test module.
- Add or keep the screenshot-shaped public rendering fixture in
  `crates/codescope-tui/tests/render.rs`.
- Update `crates/codescope-tui/tests/render.rs:93`. Its fixture already contains
  `"\t\tprefix + name"`, but the assertion expects `"+prefix + name"`. That assertion encodes the
  bug and cannot pass unchanged after correct eight-cell indentation.

## Regression plan — the required 20 cases

Use Ratatui `TestBackend` to inspect actual symbols and styles at source-body-relative buffer cells.
Helper assertions are useful, but they do not replace a buffer assertion because TestBackend itself
silently hides a leaked literal tab. For the no-control invariant, inspect the built `Line` span
contents in addition to drawing them.

1. **Git parser preserves one and two tabs.** Parse a hunk whose add/delete/context bodies start
   with `\t` and `\t\t`; assert exact `DiffLine.text.as_bytes()`.
2. **Dispatcher preserves them.** Build a `ChangeSet`, call private `selected_diff`, and pattern
   match exact `DiffRow.text` bytes for tab, double-tab, and mixed whitespace.
3. **Raw add, one leading tab.** Draw `"\tif ready {"`; assert four styled spaces at body columns
   0..4 and `i` at column 4, with gutter/sign unchanged.
4. **Raw add, two leading tabs.** Draw `"\t\treturn value"`; assert eight styled spaces and `r` at
   column 8.
5. **Delete and context parity.** Draw add/delete/context `"\tX"`; assert `X` at body column 4 and
   the correct dual line-number fields/sign for each row.
6. **Spaces unchanged.** Draw `"    X"`; assert the same four literal source spaces and `X` at 4.
7. **Mixed indentation.** Assert `"\t  value"` starts `value` at 6 and `" \tvalue"` starts it at 4.
8. **Tab after ASCII text.** Cover `a\tX -> 4`, `abc\tX -> 4`, and `abcd\tX -> 8`.
9. **Unicode cell stops.** Cover `界\tλ` (`λ` at 4), `e\u{301}\t界` (`界` at 4), and a tab
   followed by a wide grapheme. Assert buffer cells, not byte/string indices.
10. **Expanded-cell row styles.** Inspect every expanded space for Add (`ADD_BODY`), Del
    (`DEL_BODY`), and Context (`CTX_BODY`), including the expected backgrounds/modifiers.
11. **Leading-tab intraline alignment.** Pair `"\treturn oldValue"` with
    `"\treturn newValue"`; assert `return` starts at 4 with base style and the changed identifiers
    start at 11 with their delete/add highlight styles.
12. **Changed tab style.** Pair `"\treturn value"` with `"    return value"` so an equal word passes
    the unrelated-line guard. Assert all four cells expanded from the old tab use
    `DEL_HI_STYLE`, the changed new spaces use `ADD_HI_STYLE`, and `return` is base. Also retain a
    grapheme-overlap case where an original byte span touches only part of a combining cluster and
    the whole cluster is highlighted.
13. **Wrapped indentation.** Wrap a long tab-indented row. Assert the first source glyph begins at
    4, later visual lines use blank number columns plus `↪`, and no indentation is invented on a
    continuation.
14. **Wrapped width equals painted width.** With body budget 6 and `"\t界ab"`, assert the first
    body is exactly four spaces plus the two-cell `界`, with `ab` on the continuation. Also cover a
    continuation-leading tab such as `abcdef\tX` at budget 6 so it expands from that segment's zero.
15. **Horizontal scrolling across a tab.** For `ab\tXabcdefgh`, inspect windows before the tab, at
    its boundary, inside its cells, and with the right edge inside it; assert the visible tab
    remainder is styled spaces and following text retains its absolute position. Include a left
    edge bisecting `界` to protect the generic absolute-window rule.
16. **Clamp uses expanded width.** `界\t\tX` is nine display cells. With body width 5 and a very
    large requested x, assert `effective_x == 4` (and `x+04` in a full render), a fixed gutter, then
    four spaces plus `X`.
17. **Very narrow bodies terminate.** Exercise wrapped body budgets 1..3 with tab-bearing and wide
    input; assert finite output, forward progress, no empty continuation artifact, and every body
    segment at most its budget under the documented fallback.
18. **Empty and tab-only rows.** Exercise `""`, `"\t"`, and `"\t\t"` in raw and wrapped modes,
    including tiny budgets. Assert one valid logical-row anchor, correct gutters, finite styled
    whitespace chunks, and no panic/loop.
19. **No literal tab reaches source spans.** Build Add/Del/Context rows with tabs in raw and wrapped
    modes, inspect every final source `Span.content`, and assert no `\t`; then draw and assert the
    cells. Include a tab in a hunk section if the canonical diff-wide invariant covers headers.
20. **Compatibility suite.** Keep the existing Unicode, wrapping, intraline, gutter, scroll,
    geometry, and width-sweep tests passing. Specifically retain
    `diff_dual_gutter_blanks_on_the_absent_side`, `diff_gutter_fixed_during_hscroll`,
    `intraline_only_changed_words_brighten`, and `width_sweep_never_panics`; update only the stale
    integration assertion that expects tabs to disappear.

Add the requested screenshot-shaped fixture, which can implement cases 3 and 4 in one full-frame
regression while retaining focused unit cases:

```rust
DiffRow::Add {
    new_ln: 318,
    text: "\tif f.rootfs == nil {".into(),
}
DiffRow::Add {
    new_ln: 319,
    text: "\t\treturn nil, errors.New(\"rootfs client is unavailable\")".into(),
}
DiffRow::Add {
    new_ln: 320,
    text: "\t}".into(),
}
```

Compute `body_x` from the pane interior plus `gutter_width(ln_width)`. Assert `if` and `}` at
`body_x + 4`, `return` at `body_x + 8`, fixed line numbers/signs, and row backgrounds across all
expanded indentation cells. This checks the actual ResizeCreateAttachment failure shape rather
than only a helper string.

## Requirements that conflict with HEAD

1. **Literal output versus the established spec.** HEAD's `styled_graphemes`, `slice_styled`,
   `wrap_body`, and false comment directly contradict review 15's already documented expansion
   requirement.
2. **Whole-grapheme raw clipping cannot preserve tab geometry.** HEAD drops a tab when the left
   edge falls anywhere inside it and does not reserve the visible remainder. Following text shifts
   left. Partial styled spaces are required; retaining the current policy cannot be justified under
   the requested absolute viewport semantics.
3. **Strict narrow wrap versus `(usize, usize)` ranges.** HEAD admits a first tab wider than budgets
   1..3, and a grapheme-only range cannot express partial tab cells. Satisfying the strict budget
   requires presentation-cell chunks (or an explicit relaxation). The design above uses chunks.
4. **Canonical diff policy versus hunk bypasses.** Wrapped hunk strings currently keep tabs, and
   raw hunk headers use generic zero-width truncation. Either cover these small paths or explicitly
   scope the no-tab invariant to numbered source rows; covering them is safer and still
   presentation-only.
5. **"Existing tests still pass" versus a bug-encoding assertion.** The integration assertion at
   `crates/codescope-tui/tests/render.rs:93` expects `+prefix` immediately after two tabs. Correct
   rendering necessarily changes it. Preserve the test's intent with exact cell placement rather
   than preserving that assertion text.
6. **Absolute Unicode slicing.** HEAD also shifts content when x bisects a double-width grapheme
   (`界ab`, skip 1). The interval slicer can reserve the hidden remainder while it fixes tabs. This
   is a pre-existing review-16 issue, not a reason to use a tab-only coordinate model.

One unrelated audit observation should not be folded into this patch: the `build_wrapped` comment
claims a special `@@ ... @@` prefix-fit rule, while its generic `wrap_body` call does not enforce
one. Tab expansion should not silently redesign that break policy.

## Implementation verification

After implementation, run the narrow parser/dispatcher/render tests first, then the relevant
`codescope-git` and `codescope-tui` packages, formatting, `git diff --check`, the workspace tests,
and Clippy as requested by the parent task. Finally render the live ResizeCreateAttachment hunk in
raw and wrapped modes at several pane widths and horizontal offsets. Check visible nested Go
indentation, fixed gutters, correct highlight locations, and unchanged original `DiffLine`/
`DiffRow` bytes.
