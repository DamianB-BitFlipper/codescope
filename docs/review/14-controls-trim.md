# Review 14 — Trim controls after navigation becomes live

## Decision

Treat the files pane as a live master view. Moving its selection must update the diff and the
relation view; selecting a row is not a two-step “move, then activate” operation. Therefore
`Enter` should have no normal-mode binding. It remains the confirmation key inside a picker.

Keep two input dialects where they aid discovery (`j`/`k` plus arrows, `g`/`G` plus
`Home`/`End`). Remove alternate routes that add state or documentation without adding a distinct
operation. In particular, remove `Space`, `+`/`-`, `R`, `S`, `1`/`2`/`3`, and the lower-case AI
toggle `a` from the normal keymap.

## Recommended keymap

### Normal mode

| context | key(s) | exact action |
|---|---|---|
| normal mode | `q` | Quit. In a type-to-filter picker, `q` is query data instead. |
| global | `Ctrl-C` | Quit unconditionally, including while a modal is open. |
| normal/help | `?` | Open help in normal mode; close it when help is already open. In a picker, `?` is query data. |
| modal/overlay | `Esc` | Close the top modal/overlay; otherwise do nothing. |
| normal mode | `Tab` / `Shift-Tab` | Focus the next / previous pane. Three panes do not justify separate `1`/`2`/`3` aliases. |
| focused pane | `j`/`k` or `Down`/`Up` | Move one selected row in Files/Semantic, or scroll one line in Diff. |
| focused pane | `g`/`G` or `Home`/`End` | Go to the first / last row. These remain useful on large change sets and long diffs. |
| Files | `h`/`l` or `Left`/`Right` | Collapse / expand the selected file. On a symbol, Left first returns to its parent file; it must not mutate the files tree while another pane is focused. |
| Diff | `h`/`l` or `Left`/`Right` | Scroll horizontally left / right. |
| Diff | `Ctrl-u`/`Ctrl-d` | Scroll by half of the visible diff height, not a hard-coded row count. |
| Diff | `PgUp`/`PgDn` | Scroll by one visible diff page. |
| Diff | `n` / `N` | Jump to the next / previous actual hunk header. |
| normal mode | `s` / `u` / `B` / `w` | Select staged / unstaged / branch-vs-base / working scope directly. |
| normal mode | `b` | Open the comparison-base picker. |
| normal mode | `m` | Open the AI-model picker. |
| normal mode | `A` | Explicitly generate or refresh the AI view for the current repository state. This remains manual because it is a remote, potentially billed data transfer. |

The direct scope keys earn their place because each chooses a different, commonly used input to
analysis in one step. Remove `S`: cycling is a second route to the same four states and can start
work for unwanted intermediate scopes. By contrast, direct pane numbers save at most two cheap
`Tab` presses, so `1`/`2`/`3` do not earn three permanent bindings.

Page and hunk controls should be focus-local. The current `HalfPage*`, `Page*`, and `NextHunk` /
`PrevHunk` actions can change the diff even when another pane has focus. A focused-pane grammar is
more predictable and avoids invisible mutations in compact layouts.

### Picker and help modal grammar

Both `b` and `m` must open the same kind of picker and use the same controls:

| key(s) | picker action |
|---|---|
| any plain character, including `j`, `k`, `b`, `m`, and `q` | Append to the filter query. |
| `Backspace` | Delete one filter character. |
| `Up` / `Down` | Move through filtered results. |
| `Enter` | Apply the highlighted result and close. With no result, do nothing and remain open. |
| `Esc` | Cancel and close without changing the current base/model. |
| `Ctrl-C` | Quit the application. |

Do not use `j`/`k` for picker movement while also claiming that users can type to filter. The
current `picker_key` reserves those letters, so refs or models containing `j` or `k` cannot be
searched faithfully. Arrow-only movement inside a text-entry modal is the smallest consistent
rule. The opener is not also a close key: after a picker opens, `b` and `m` are ordinary query
characters, and `Esc` is the single cancel path.

Help is not a picker. `?` opens it, and `?` or `Esc` closes it. No other normal-mode action should
leak through a modal, except unconditional `Ctrl-C`.

## What becomes automatic

1. **Files selection drives both detail panes.** After any actual Files selection change from
   `j`/`k`, arrows, `g`/`G`, `Home`/`End`, or a tree navigation step, update the diff immediately.
   A symbol row also requests its relations automatically. A file row shows that file's diff and
   clears or replaces symbol-specific relations.
2. **Initial and replacement snapshots select a view.** The first valid row, and the reconciled
   row after a scope/base/repository refresh, must drive the same update without a synthetic
   `Enter`.
3. **Repository refresh is automatic.** Startup already triggers a refresh, filesystem and git
   state are watched, and changing scope or base starts analysis. A permanent `R` binding is not
   needed. If a transient failure needs manual recovery, show a contextual `retry` action only in
   that error state rather than advertising refresh during normal operation.
4. **Selection work is latest-wins.** Rapid navigation must coalesce/debounce relation requests
   and tag results with the selected entity (or a selection generation). A slow response for an
   old symbol must never replace the relations for the row now highlighted.
5. **AI is deliberately not automatic.** `A` is the explicit network boundary. Model selection
   may update the chosen model, but it should not send repository-derived content until `A`.

This makes normal `Enter` redundant. `Action::Activate` currently forwards a symbol only from a
symbol row and then calls `App::activate`, which merely expands a file; it is a no-op in the other
panes. Once `SelectionChanged` owns live detail updates, remove `Activate` and its normal-mode
mapping rather than keeping a vague “re-center” action. If codescope later gains a real “open this
symbol in an editor” or “follow this relation” command, `Enter` can return for that distinct,
implemented operation.

## Remove or repair the current controls

### Remove `Space` / `ToggleExpand`

`Space` and `h`/`l` are two ways to change the same file expansion bit. Keep the directional pair:
it also serves horizontal diff scrolling and has a conventional arrow equivalent. `Collapse` and
`Expand` must be Files-only for tree state; today the non-Diff branch also changes the Files tree
while Semantic has focus.

Expansion state must survive snapshot publication. `App::update` replaces the snapshot, while the
dispatcher currently rebuilds every `FileRow` with `expanded: true`. Navigation-driven relation
publishes will make this reset happen even more often. Store open file paths in `App`, or reconcile
incoming rows by stable path. If expansion cannot persist, remove folding entirely rather than
advertise a control whose result immediately disappears.

### Remove `+` / `-` / `ExpandMore` / `ExpandLess`

`sem_depth` is incremented and decremented but read nowhere outside tests. The semantic pane is a
flat set of rows, so “default semantic depth” has no visible meaning. Use a bounded automatic
relation query now. If a real expandable relation tree arrives later, expand individual nodes with
Left/Right instead of restoring a hidden global depth setting.

### Remove `R` / `RefreshGit`

Repository watching is the normal refresh mechanism, and scope/base changes already request new
analysis. A manual rescan key makes users wonder when the screen can be trusted. Recovery from an
explicit error can be contextual; it does not need a normal-mode binding.

### Remove `a` / `AiToggle`; keep `A`

At present `a` only gates whether `A` can start work. It does not itself generate a view, and it
does not reliably remove an already rendered AI view. Because AI generation is already an
explicit `A` action, a second enable bit adds no useful consent boundary. Keep `--no-ai` for a
session-wide hard disable and keep `A` as the deliberate per-view operation. Restore a runtime
toggle only if AI ever becomes automatic, in which case it must actually pause work and remove or
clearly mark the AI presentation.

### Keep `g` / `G`

They are not duplicates of hunk navigation. They bound long lists and diffs in one action, and
`Home`/`End` keeps the same operation discoverable to non-Vim users. A Files jump emits the same
selection-change event as one-row movement.

### Keep `n` / `N`, but make them real

Hunk navigation is a high-value diff operation. The current `jump_hunk` changes only
`current_hunk`, which updates the title counter but not `diff_scroll`. Make it find the target
`DiffRow::HunkHeader`, scroll that row into view, and derive the displayed hunk number from the
viewport. If that work is not done, remove `n`/`N` from the keymap and help until it is; a moving
counter is worse than no shortcut.

## Documentation and presentation changes

The keymap must have one contract across `map_key`, the help modal, the footer, and README:

- Remove normal-mode `Enter`, `Space`, `+`/`-`, `R`, `S`, `1`/`2`/`3`, and `a` everywhere.
- Add `m` to the help modal and README keymap table. It is currently only mentioned later in the
  README's AI prose and is absent from the rendered help.
- Change the README base-picker instructions from `j`/`k` movement to arrow movement, and document
  the shared type-to-filter grammar once for both pickers.
- Describe `A` as an explicit AI generate/refresh, not half of an `a`/`A` toggle pair.
- Make the footer contextual: show Files folding keys when Files is focused and diff paging/hunk
  keys when Diff is focused. Keep the always-visible part short (`q quit · ? help · Tab pane`).

A regression test should enumerate every help/table binding and assert that it maps in the stated
context. Separate tests should assert that removed normal keys map to `Action::None`, while
`Enter` still confirms both pickers and every plain letter remains valid picker input.
