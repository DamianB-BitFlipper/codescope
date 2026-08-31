# 15 — Codescope reference-image redesign specification

## Decision

Replace the current files/diff/semantic tier layouts with one clear master-detail layout at normal
terminal sizes:

```text
 codescope  repo  branch ◂ base                         branch  LSP ✓  AI × prime
 83 changed files · 12 symbols in executor.go · hunk 1 / 2
┌ Changed files ─────────────────────── 83 ┐┌ executor.go ─ RecoverExistingCreate · hunk 1/2 · wrap off ┐
│M ▾ …/actionworker/executor.go       12 ││  20 │      - old source                                  │
│    RecoverExistingCreate               ││     │   20 + new source                                  │
│A ▸ …/runtime/config.go               3 ││@@ -20,7 +20,7 @@ interface lifecycleOwner               │
│                                           ││  21 │   21   unchanged context                           │
└───────────────────────────────────────────┘└──────────────────────────────────────────────────────────┘
┌ Impact ───────────────────────────────────────────────────────────────────────────────────────────────┐
│ SELECTED CHANGE                         │ CALLERS · 3                  │ DOWNSTREAM · 2                 │
│ RecoverExistingCreate  modified         │ CreateExecutor               │ persistCreate                   │
│ Recovers an existing create operation.  │ Reconcile                    │ notifyWaiters                   │
└───────────────────────────────────────────────────────────────────────────────────────────────────────┘
 AI timed out after 20s · A retry · m change model · deterministic impact remains available
 Tab pane · z zoom · W wrap · n/N hunk · [ / ] resize · ? help
```

The deterministic Impact pane is permanent. AI may improve the interpretation sentence, but it
must never replace or hide deterministic callers/downstream data. The reference layout is the
contract at `width >= 80 && height >= 20`. Smaller terminals use a focus-only fallback described
below.

## 1. Exact Ratatui geometry

All dimensions below are terminal cells and include pane borders. Do not add outer margins.

### 1.1 Normal layout (`width >= 80`, `height >= 20`, not zoomed)

```rust
let rows = Layout::vertical([
    Constraint::Length(1), // top repository/service bar
    Constraint::Length(1), // summary bar
    Constraint::Min(7),    // files + diff; receives all surplus height
    Constraint::Length(9), // full-width Impact pane, including its border
    Constraint::Length(1), // status message
    Constraint::Length(1), // compact help
]).split(area);
```

This gives these exact rectangles at `140x40`: top `y=0`, summary `y=1`, work `y=2..28`
(`height=27`), Impact `y=29..37` (`height=9`), status `y=38`, and help `y=39`.
At the minimum `80x20`, work is seven rows and Impact is nine rows.

Split the work row as follows:

```rust
// App-owned and changed by '[' / ']'.
const DEFAULT_FILES_WIDTH: u16 = 42;
const MIN_FILES_WIDTH: u16 = 28;
const MAX_FILES_WIDTH: u16 = 56;
const MIN_DIFF_WIDTH: u16 = 48;

// This subtraction is safe in the normal tier because work.width >= 80.
let files_width = app.files_width
    .clamp(MIN_FILES_WIDTH, MAX_FILES_WIDTH)
    .min(work.width - MIN_DIFF_WIDTH);
let work_cols = Layout::horizontal([
    Constraint::Length(files_width),
    Constraint::Min(MIN_DIFF_WIDTH),
]).split(rows[2]);
```

`App::files_width` defaults to 42. `[` subtracts two cells and `]` adds two cells, clamped to
`28..=56`. Add `Action::ResizeFilesNarrower` and `Action::ResizeFilesWider`; this is view state,
not a snapshot field. If a resize makes the requested width impossible, the renderer applies the
`MIN_DIFF_WIDTH` clamp above without changing the stored preference.

Render both work panes with `Block` and the Impact pane with one full-width `Block`. The sizes
above include those borders. A straightforward `Borders::ALL` implementation is acceptable; do
not put a third semantic column beside the diff.

Inside `Impact`'s `Block::inner(area)`, use:

```rust
let impact_cols = Layout::horizontal([
    Constraint::Percentage(40), // selected change
    Constraint::Percentage(30), // callers
    Constraint::Percentage(30), // downstream
]).split(impact_inner);
```

Use a right border on the first and second inner column blocks and no border on the third. Give
all three `Padding::horizontal(1)`. Their first interior row is the column header; the remaining
rows hold content. Do not create three separately bordered outer panes.

### 1.2 Focus-only and small-terminal behavior

Keep the existing hard stop at `width < 30 || height < 8`: render only
`terminal too small (WxH)`.

For `width < 80`, `height < 20`, or `app.zoomed`, retain the chrome and render only the focused
pane in the body. `Tab`/`BackTab` and `1`/`2`/`3` switch Files, Diff, and Impact. Use:

```rust
// height >= 12
Layout::vertical([
    Constraint::Length(1), // top
    Constraint::Length(1), // summary
    Constraint::Min(3),    // focused pane
    Constraint::Length(1), // status
    Constraint::Length(1), // help
])

// height 8..=11: help is the first row dropped
Layout::vertical([
    Constraint::Length(1), // top
    Constraint::Length(1), // summary
    Constraint::Min(3),    // focused pane
    Constraint::Length(1), // status
])
```

Zoom uses this same focus-only body even on a large terminal. It keeps top, summary, status, and
help visible. Rename `Pane::Semantic` to `Pane::Impact` (or retain the enum name only as a temporary
internal migration alias); no visible label should say "Semantic".

This replaces `Tier::Medium`, `Tier::TallStack`, and `Tier::Spacious`. Keeping those tiers in
parallel would produce two incompatible information architectures.

## 2. Palette and common widget rules

Define one palette in `render.rs` rather than scattering `Color::Green`/`Color::Red`:

| Token | Ratatui color | Use |
|---|---|---|
| `SURFACE` | `Color::Rgb(24, 27, 32)` | top, status, help backgrounds |
| `SURFACE_ALT` | `Color::Rgb(31, 35, 41)` | summary and hunk-header bands |
| `TEXT` | `Color::Rgb(210, 214, 220)` | normal labels/source |
| `MUTED` | `Color::Rgb(122, 128, 139)` | context, paths, separators, gutters |
| `BORDER` | `Color::Rgb(67, 73, 83)` | unfocused borders and inner dividers |
| `ACCENT` | `Color::Rgb(91, 166, 255)` | focused border, product/repo, active symbol |
| `SELECTED_BG` | `Color::Rgb(46, 54, 66)` | active list row |
| `OWNER_BG` | `Color::Rgb(35, 41, 50)` | owning file while a child symbol is active |
| `ADD_FG` | `Color::Rgb(100, 190, 120)` | `A`, `+`, added-line accent |
| `ADD_BG` | `Color::Rgb(27, 49, 35)` | restrained added-line body |
| `ADD_HI` | `Color::Rgb(151, 232, 166)` | changed word in an added line |
| `ADD_HI_BG` | `Color::Rgb(46, 88, 58)` | changed-word emphasis |
| `DEL_FG` | `Color::Rgb(225, 113, 122)` | `D`, `-`, removed-line accent |
| `DEL_BG` | `Color::Rgb(58, 30, 35)` | restrained removed-line body |
| `DEL_HI` | `Color::Rgb(255, 166, 172)` | changed word in a removed line |
| `DEL_HI_BG` | `Color::Rgb(101, 45, 53)` | changed-word emphasis |
| `WARN` | `Color::Rgb(218, 174, 86)` | modified status, warnings, stale/loading |
| `HUNK_FG` | `Color::Rgb(132, 190, 229)` | hunk-header text |
| `ERROR` | `Color::Rgb(238, 95, 101)` | failures/diagnostics |

Use `Modifier::BOLD` only for product, selected/basename labels, column headings, and intraline
changed spans. Do not use `REVERSED`: it destroys the deliberate red/green diff palette. A focused
pane gets an `ACCENT` border; an unfocused pane uses `BORDER`. Selection remains visible when its
pane is unfocused by keeping `SELECTED_BG` and dropping only bold if desired.

For left/right pane titles, use multiple block titles:

```rust
Block::default()
    .borders(Borders::ALL)
    .title(Line::from(left_title).left_aligned())
    .title(Line::from(right_title).right_aligned());
```

Pre-elide the two titles so they never overlap. Ratatui does not resolve overlapping titles for us.

## 3. Region-by-region contract

### 3.1 Top bar

**Exact normal-width text**

- Left: `codescope  {repo_name}  {branch} ◂ {base}`.
- Right: `{scope}  LSP {ls_glyph}  AI {ai_glyph}[ {provider}]`.
- Append `  ⟳` to the right group while `snap.refreshing` is true.

The first right-hand `branch` in the reference is the active change scope, not the Git branch a
second time. Use the full scope words `branch`, `staged`, `unstaged`, and `working`.

Reserve the right group first with
`Layout::horizontal([Constraint::Min(1), Constraint::Length(right_width)])`. Truncate/elide the
left group after measuring the right. Keep the current grapheme-safe `truncate_cells` helper.
The drop order is base, Git branch, repo, then product; service failures and refresh state are
never clipped.

Glyph mapping:

- LSP: ready `✓`/green; starting or indexing `…`/WARN; degraded `~`/WARN; failed `×`/ERROR.
- AI: ready `✓`/green; loading `…`/WARN; stale `~`/WARN; disabled `×`/MUTED; failed `×`/ERROR.
- Show the provider (`prime`, `openai`, and so on) whenever configured, including when AI is
  toggled off. Do not show the long model in this compact bar; `m` exposes it.

**Data:** all required fields already exist: `repo`, `scope`, `base_ref`, `ls`, `ai`,
`ai_provider`, and `refreshing`. `scope_counts` is not needed for this reference bar and is still
unwired today.

### 3.2 Summary bar

Render a full-width `Paragraph` with `SURFACE_ALT` background and one leading space:

```text
83 changed files · 12 symbols in executor.go · hunk 1 / 2
```

Use the active scope's `snap.files.len()`. The symbol count belongs to the file displayed in the
diff, not to the whole repository. Match `snap.diff.file_path` (currently `DiffPane.title`) to its
`FileRow`; do not trust a transient flat list index. Use the basename for `executor.go` and retain
the full path for the status row. The hunk index used here must be the same effective index used
in the diff title.

Empty states:

- no files: `0 changed files · no selection`;
- selected file with no mapped symbols: `N changed files · 0 symbols in file.ext`;
- no hunks: omit the final hunk phrase.

At constrained widths preserve `N changed files` and the hunk phrase first; elide the middle file
phrase. Apply singular grammar for `1 changed file` and `1 symbol`.

**Data:** `files.len()` and `DiffPane::{current_hunk,total_hunks}` exist. Add
`FileRow::changed_symbol_count: usize` (**NEW SNAPSHOT FIELD**) and use it instead of assuming that
`symbols.len()` is forever complete. It equals `symbols.len()` today, but remains correct if child
rows later become lazy or capped. See the hunk-state correction in section 5.

### 3.3 Changed files pane

The outer title is exactly `Changed files` on the left and the active file count on the right.
Do not put the shared directory root in the title.

A file row has this cell structure:

```text
{status} {disclosure} {display_path}{padding >= 1}{changed_symbol_count}
```

- status: one cell (`M`, `A`, `D`, `R`, `?`, `U`);
- disclosure: `▾` expanded or `▸` collapsed;
- count: right-aligned to the largest count width currently visible;
- path: directory components in `MUTED`, basename in `TEXT` (bold on the active file).

With `inner_width = area.width - 2`, the path budget is:

```rust
let count_width = digits(max_file_symbol_count).max(1);
let path_budget = inner_width.saturating_sub(5 + count_width);
// status + space + disclosure + space + at-least-one gap = 5 fixed cells
```

Run path elision over the complete file set, not independently per row. Keep full repo-relative
paths as identity. Update `elide.rs` so it:

1. compares whole path components;
2. strips a worthwhile common directory prefix and leaves a visible `…/` marker in every row;
3. preserves the shortest component suffix that distinguishes duplicate basenames;
4. middle-elides only after preserving that suffix; and
5. adds a stable `·01`, `·02` ordinal only if extreme width still makes two display strings equal.

The existing component-aware shared-root and grapheme-width code is reusable. The current helper
does not implement the final collision ordinal and currently budgets no right-hand count.

Expanded symbol rows are indented four cells. Render a change glyph (`+`, `~`, `-`) followed by
the symbol name; keep confidence and diagnostic markers dim/red at the right only when they fit.
The active flattened row gets `SELECTED_BG`. If a symbol child is active, give its owning file row
`OWNER_BG`, so the selected file remains obvious.

Status colors: `A`/`?` use `ADD_FG`, `M` uses `WARN`, `D` uses `DEL_FG`, `R` uses `ACCENT`, and `U`
uses `ERROR`.

**Data:** path, status, symbols, expansion, symbol change/confidence/diagnostic already exist.
`changed_symbol_count` is new. Expansion is currently incorrectly snapshot-owned and can reset on
publish; moving expanded paths into `App` is recommended but not required to draw the first
prototype.

### 3.4 Diff pane

#### Title

The left title is the selected file's basename only. The right title is:

```text
{focused_symbol} · hunk {current}/{total} · wrap {on|off}
```

Omit the symbol and its separator on a file-row selection. When raw horizontal scroll is nonzero,
append ` · x+NN`. Preserve `hunk` and `wrap` first, elide the symbol second, and elide the basename
last. Add `DiffPane::focused_symbol: Option<String>` (**NEW SNAPSHOT FIELD**); the current dispatcher
owns `selected_symbol` but does not publish it. Derive the basename in the renderer—do not add a
second copy of the path.

The reference/default mode is `wrap off`; change `App::new()` to start with `diff_wrap = false`.
`W` toggles it.

#### Dual fixed gutter

Do not use a `Table`; retain the current prebuilt `Vec<Line>` and logical-to-visual scroll map.
Choose `ln_width = max(4, digits(max line number)).min(6)` per diff. Every source row starts with:

```text
{old:>ln_width} │ {new:>ln_width} {sign}{source}
```

Examples:

```text
  20 │      -return old
     │   20 +return new
  21 │   21  unchanged context
```

An absent old/new number is exactly `ln_width` spaces. The fixed gutter width is
`2 * ln_width + 5` cells (`old`, `" │ "`, `new`, one space, and the sign). Keep the entire gutter
fixed during horizontal scroll; slice only source spans. Continuation lines use a blank dual
gutter and put `↪` in the sign cell. Context line numbers and source use `MUTED`.

No `DiffRow` field is missing for dual gutters: add/del rows already provide one side, and context
rows provide both.

#### Hunk header band

A `DiffRow::HunkHeader` renders without number fields as one full-inner-width line. Pad it to the
right, then apply `fg(HUNK_FG).bg(SURFACE_ALT).bold()` to the complete band:

```text
@@ -20,7 +20,7 @@ interface lifecycleOwner
```

Hunk headers do not horizontal-scroll. In raw mode, truncate the section text with an ellipsis. In
wrap mode, a header may wrap only if the `@@ ... @@` prefix itself fits; every continuation keeps
the same band background.

#### Added, removed, context, and intraline styling

- Added line: sign/gutter accent `ADD_FG`; source `TEXT` on `ADD_BG`.
- Removed line: sign/gutter accent `DEL_FG`; source `TEXT` on `DEL_BG`.
- Context: number gutter and source `MUTED`, no colored background.
- Changed words in a paired add/del: `ADD_HI`/`DEL_HI`, brighter matching background, and bold.
  Equal words retain the restrained line style.

Compute intraline spans in a pure TUI helper; this is display derivation and needs no dispatcher or
snapshot field. The exact pairing algorithm is:

1. Within one hunk, find each maximal deletion run immediately followed by an addition run.
2. Align those runs monotonically with `similar::TextDiff::from_lines` (the workspace already has
   `similar`). Within each replace operation, pair lines by relative order; unmatched lines keep
   only whole-line styling.
3. For each candidate pair, run `TextDiff::from_words`. Apply intraline emphasis only when the
   pair retains at least one equal word; unrelated replacement lines must not become one giant
   bright block.
4. Mark delete segments only on the old line and insert segments only on the new line. Preserve
   text and whitespace exactly.

The existing `wrap_body(&str)` and `slice_cells(&str)` lose span boundaries. Replace them with
span-aware helpers that walk graphemes carrying a style, form new `Span`s when style changes, and
return both wrapped/sliced text and its styles. Expand tabs to four-cell stops while building
visual spans so measurement and terminal output agree. Keep `BuiltDiff::first_visual` so
`diff_scroll` remains a logical-row anchor.

This span-aware wrap/slice work is the hardest renderer change; coloring the whole line first and
adding intraline later is not an acceptable final state.

### 3.5 Impact pane

The outer block title is exactly `Impact`. Its three headers are:

- `SELECTED CHANGE`
- `CALLERS · {N}`
- `DOWNSTREAM · {N}`

Use `MUTED + BOLD` for headers. On loading, show `CALLERS · …` or `DOWNSTREAM · …`, not a false
zero. Lists show at most the available interior rows. If rows remain, the final visible row is
`… +N more` in `MUTED`.

`SELECTED CHANGE` shows:

1. symbol label in `ACCENT + BOLD` plus added/modified/removed badge;
2. exactly one interpretation line, truncated by display cells;
3. an optional partial/loading note in `MUTED` if space remains.

If a file row rather than a symbol is selected, show the basename and
`N changed symbols in this file; select one to inspect impact.` Callers and downstream are empty.

For the prototype, define **downstream** honestly as immediate outgoing relationships only:
callees plus outgoing one-hop impact-graph neighbors. Preserve a small relation suffix (`calls`,
`implements`, `references`, and so on) instead of implying every row is a transitive effect.
Do not claim multi-hop impact; the analysis graph is intentionally one hop.

The deterministic interpretation can be built from `ChangedSymbolInfo`:

- added: `Added {kind} across N hunks.`
- modified with `signature_touch`: `Modified signature and implementation across N hunks.`
- other modified: `Modified implementation across N hunks.`
- removed: `Removed {kind}; callers may require updates.`

AI may replace that sentence only when a validated, current-epoch result is explicitly tied to the
same selected entity. The current `VisualizationPlan.forms[0].summary` is repository-wide and must
not be mislabeled as a selected-symbol interpretation. Until selection-specific AI output exists,
show the deterministic sentence. AI failure never clears this pane.

**Data:** the current flattened `SemanticPane` cannot represent these three simultaneous concerns.
Add the structures in section 4 and have the dispatcher publish them. Current lazy
`selected_relations` already has callers and callees but must be kept as separate vectors. The
refresh-time `ImpactGraph` supplies one-hop outgoing relations and completeness.

### 3.6 Status message bar

Reserve this row even when there is no error. Render `snap.status.text`; if empty, show the selected
file's full repo-relative path in `MUTED`. The row uses `SURFACE`; info text is `MUTED`, warnings
use `WARN`, and errors use `ERROR`.

For an AI timeout/failure, the dispatcher must publish an actionable, sanitized message:

```text
AI timed out after 20s · A retry · m change model · deterministic impact remains available
```

Use the real timeout/reason, not a hard-coded 20. The suffix is valid because `A` already requests
an AI refresh and `m` opens the model picker. Never include request bodies, URLs, headers, or API
keys.

The existing `UiSnapshot.message: String` is wired, but it has no severity and current AI failures
are formatted only as `AI: {reason}`. Keep `message` and add `message_level`, or replace it with the
typed `StatusMessage` below. This is **NEW SNAPSHOT/DISPATCHER WIRING**.

### 3.7 Compact help bar

Render styled spans, not one undifferentiated string. Key tokens use `TEXT`; explanations and
separators use `MUTED`. At `width >= 96`, use exactly:

```text
Tab pane · z zoom · W wrap · n/N hunk · [ / ] resize · ? help
```

At `64..95`, drop resize first:

```text
Tab pane · z zoom · W wrap · n/N hunk · ? help
```

At `30..63`, use `Tab · z · W · n/N · ?`. The help modal remains the place for quit, scope,
AI, base, movement, and picker bindings.

**Data:** none from `UiSnapshot`; this uses the keymap and `App` state only.

## 4. Required snapshot shape

The following is the minimum explicit rendering model. Names may vary, but do not flatten the
three Impact columns back into one `Vec<SemRow>`.

```rust
pub struct UiSnapshot {
    // existing fields ...
    pub files: Vec<FileRow>,
    pub diff: DiffPane,
    pub impact: ImpactPane,                 // NEW; replaces rendered SemanticPane
    pub status: StatusMessage,              // NEW; replaces message, or add level beside message
}

pub struct FileRow {
    pub path: String,
    pub status: &'static str,
    pub changed_symbol_count: usize,        // NEW
    pub symbols: Vec<SymbolRow>,
    pub expanded: bool,
}

pub struct DiffPane {
    pub file_path: String,                  // rename current ambiguous `title`, or keep title
    pub focused_symbol: Option<String>,     // NEW
    pub rows: Vec<DiffRow>,
    pub total_hunks: usize,
    // current_hunk should become App view state; see below
}

#[derive(Debug, Clone, Default)]
pub struct ImpactPane {
    pub selected_change: Option<SelectedChange>,
    pub callers: ImpactList,
    pub downstream: ImpactList,
    pub note: String,
}

#[derive(Debug, Clone, Default)]
pub struct ImpactList {
    pub rows: Vec<ImpactRow>,
    pub state: ImpactLoadState,             // Idle, Loading, Ready, Unavailable
    pub partial: bool,
}

pub struct SelectedChange {
    pub file: String,
    pub label: String,
    pub change: &'static str,               // added / modified / removed
    pub interpretation: String,
    pub interpretation_source: InterpretationSource, // Deterministic / Ai
}

pub struct ImpactRow {
    pub label: String,
    pub relation: &'static str,
    pub changed: bool,
    pub has_diagnostic: bool,
}

pub struct StatusMessage {
    pub text: String,
    pub level: StatusLevel,                 // Info / Warning / Error
}
```

`ImpactLoadState` is necessary to distinguish "zero callers" from "callers have not returned".
Defaults must produce an empty but renderable frame.

### Hunk state ownership

`DiffPane.current_hunk` is currently dispatcher data that `App::jump_hunk` mutates locally; any
new snapshot resets it to 1. Correct this while both summary and title depend on it:

- add `App::current_hunk: usize` (1-based, 0 when there are no hunks);
- reset it when `DiffPane.file_path` changes;
- `NextHunk`/`PrevHunk` update it and `diff_scroll` together;
- ordinary vertical scrolling recomputes it as the greatest hunk-header row at or before the
  logical scroll anchor; and
- both summary and diff title read this single App value.

The hunk header offsets can be scanned from `DiffRow::HunkHeader` as today; no extra dispatcher
field is required. `total_hunks` remains immutable snapshot data.

## 5. Dispatcher wiring map

All new backend wiring belongs in `crates/codescope/src/dispatcher.rs`; the renderer must remain
I/O-free.

1. **`file_rows()`**: after building each file's symbol vector, set
   `changed_symbol_count = symbols.len()` before moving the vector into `FileRow`.
2. **`selected_diff()` / `panes()`**: pass the dispatcher's `selected_symbol` label and publish it
   as `DiffPane.focused_symbol`. Keep the full path as identity; basename is render-only.
3. **`on_selection_changed()`**: immediately publish the new `SelectedChange`. On a symbol row,
   set callers/downstream to `Loading`, clear the previous entity's rows, and call `spawn_expand`.
   On a file row, publish the file-level selected-change fallback and leave lists `Idle`.
4. **`RelationsLoaded`**: epoch- and identity-gate exactly as now, then store callers and callees
   separately, set their states to `Ready`, and publish. Change `relations_for()` so it does not
   discard `Evidence.completeness`; map incomplete evidence to `partial = true`.
5. **Impact graph merge**: locate the selected graph node by file plus qualified symbol. Merge
   incoming `Calls` neighbors into callers and outgoing neighbors into downstream, deduplicate by
   `(label, relation)`, and keep stable label order. Lazy LSP call hierarchy wins on duplicates.
6. **Selected interpretation**: find the exact `ChangedSymbolInfo` using file, name, and selection
   position. Build the deterministic sentence from `kind`, `record.change_kind`,
   `record.hunks.len()`, and `signature_touch`.
7. **`panes()` refactor**: stop choosing one of relations, AI, or graph. Build one `ImpactPane`
   containing deterministic selected change + relations + graph. AI rows must not replace it.
   `SemanticPane` can be removed after picker/help tests migrate.
8. **`on_ai_done()`**: retain validated plan metadata if needed, but use AI prose in
   `SelectedChange.interpretation` only when it is selection-specific and epoch-matched. Otherwise
   leave the deterministic sentence intact.
9. **Status**: map analysis/LSP/AI failures and picker feedback to `StatusMessage { text, level }`.
   For every AI failure append the retry/model/deterministic-fallback suffix. Keep existing epoch
   gates and sanitized error strings.
10. **`build_snapshot()` and defaults/tests**: populate `impact`, typed status,
    `changed_symbol_count`, and `focused_symbol` on every path, including git-only and placeholder
    snapshots.

Data that still cannot be made stronger without new analysis work:

- `ScopeCounts` is always default today; the target does not require per-scope counts.
- Downstream is only one hop. Deeper transitive impact needs additional lazy analysis and must not
  be implied by the label.
- Current AI summaries are global, not selected-entity interpretations.
- Selected-symbol-to-hunk identity is not published; the target needs only current/total hunk and
  can use existing row offsets.

## 6. Difficulty and implementation order

| Work | Difficulty | Reason |
|---|---|---|
| New outer layout, top/summary/status/help bars | Easy | Pure `Layout`, `Paragraph`, and measured spans. |
| Files title/count, status colors, right-aligned per-file count | Easy | Existing `List` and `elide` helpers; one new field. |
| Hunk band and dual line-number gutter | Medium | Data exists, but every wrap/raw prefix and width calculation changes. |
| Three-column Impact renderer | Medium | Widget work is easy; empty/loading/truncation states need care. |
| Splitter actions and App state | Easy | Two actions, two bindings, one clamped `u16`. |
| Snapshot/dispatcher Impact triple | Medium–hard | Must merge selection, lazy LSP evidence, graph evidence, loading, and epoch gates without stale rows. |
| Correct hunk view-state ownership | Medium | Current value is mutable snapshot state and resets on publish. |
| Intraline changed-word rendering | Hard | Pairing plus styled-grapheme wrap/slice must preserve fixed gutters, tabs, and logical scroll anchors. |
| Honest AI interpretation | Hard beyond deterministic v1 | Current AI summary is global; selection-specific provenance is missing. |

Recommended landing order: (1) snapshot types/defaults, (2) dispatcher Impact assembly, (3) outer
layout and bars, (4) files/Impact widgets, (5) dual gutters/hunk band, (6) intraline span pipeline,
(7) responsive/zoom and visual regression tests.

## 7. Acceptance tests

Use `TestBackend` assertions on content **and styles**, not only "does not panic".

1. At `140x40`, assert the exact row geometry from section 1 and default x split: Files width 42,
   Diff width 98, Impact full width at `y=29..37`, status `y=38`, help `y=39`.
2. At `80x20`, assert Files width 32, Diff width 48, work height 7, and Impact height 9.
3. At `79x40` and `140x19`, assert focus-only rendering; Tab changes the visible pane. At `29x8`
   and `30x7`, assert the too-small message. At `30x8`, assert no panic and no invalid rectangle.
4. Assert top text contains `codescope`, repo, `branch ◂ base`, and right-reserved
   `branch  LSP ✓  AI × prime`; a long branch must not clip service state.
5. Assert the summary count follows the diff-selected file, including selection on a nested symbol,
   and that hunk values in summary and diff title always match.
6. Assert the Files block has left title `Changed files`, right count, selected row background,
   colored `M`/`A`, an indented symbol, and a right-aligned per-file count.
7. Use duplicate long basenames and Unicode paths; display strings must remain distinct, valid
   graphemes, and within the computed budget.
8. Assert add/del/context rows show both old and new gutter columns with blanks on the absent side.
   Horizontal scroll must leave both columns and the sign fixed.
9. Assert the hunk header background reaches every interior cell in the band.
10. For `return oldValue` → `return newValue`, assert `return ` has the restrained line style and
    only `oldValue`/`newValue` cells have the bright intraline style. Test unequal and unrelated
    del/add runs, wrapping, Unicode, tabs, and raw slicing.
11. Assert Impact always contains all three headers and correct counts. While relations load,
    assert `· …`; after completion assert `· N`. A stale `RelationsLoaded` event must not change
    the pane.
12. Assert an AI failure leaves deterministic Impact rows visible and shows the actionable status
    suffix. Assert no secret/request content reaches the buffer.
13. Assert `[`/`]` resize in two-cell steps, clamps at 28/56, survives terminal resize, and is
    listed in help.
14. Assert `z` zooms Files, Diff, or Impact while retaining all four chrome rows; resizing and Tab
    while zoomed remain deterministic.
