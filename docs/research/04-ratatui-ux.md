# 04 — Ratatui interaction design for codescope

Research date: 2026-08-29. All version/API claims verified locally: every pattern below
was compiled and unit-tested against a scratch crate (`ratatui 0.30.2` + `crossterm 0.29.0`
+ `tokio 1.53` + `tui-tree-widget 0.24.1`, `cargo check --all-targets` + `cargo test` pass),
and behavior claims were checked against vendored crate sources in `~/.cargo/registry`.

## 1. Stack and versions (verified on crates.io)

| crate | version | notes |
|---|---|---|
| `ratatui` | **0.30.2** | facade over `ratatui-core 0.1.x`, `ratatui-widgets 0.3.x`, `ratatui-crossterm 0.1.x` |
| `crossterm` | **0.29.0** | features: `event-stream` (`dep:futures-core`, needs `events`/mio) — verified in feature list |
| `tokio` | 1.53.x | `rt-multi-thread`, `macros`, `sync`, `time` |
| `futures` | 0.3 | for `StreamExt` on `EventStream` (no need for `futures-util` separately) |
| `tui-tree-widget` | **0.24.1** | declares `ratatui ^0.30`; compiled + rendered in TestBackend test |

Pitfall: ratatui 0.30's default backend feature maps to crossterm **0.29**
(`ratatui-crossterm/default = ["crossterm_0_29"]`). The app's direct `crossterm` dep must be
0.29.x; a mismatch compiles a second crossterm copy and `EventStream`/`Event` types stop
unifying. `event-stream` must be enabled on our direct dep (features do not propagate through
the `ratatui::crossterm` re-export).

## 2. App structure with tokio

Use `ratatui::init()` / `ratatui::restore()`, not manual raw-mode/alt-screen calls:

```rust
let terminal = ratatui::init();   // -> DefaultTerminal = Terminal<CrosstermBackend<Stdout>>
let res = run(terminal).await;    // app loop
ratatui::restore();
res
```

- Panic safety: `init()`/`run()` **install their own panic hook** (verified in
  `ratatui-0.30.2/src/init.rs:567`: hook calls `restore()` then the previous hook). If we add
  `color-eyre`/`better-panic`, install that hook *before* `ratatui::init()` so init wraps it.
- `ratatui::run(closure)` exists but takes `FnOnce(&mut DefaultTerminal)` — sync closure,
  wrong shape for our tokio loop. Use explicit `init`/`restore`.
- `try_init()`/`try_restore()` are the `Result` variants; `init()` panics on failure.
  For a TUI, panicking at startup on `init` failure is acceptable; use `try_init` if we want
  a clean "not a TTY" message.

Event loop (compiled-verified shape):

```rust
let mut events = EventStream::new();          // crossterm, requires event-stream feature
let mut tick = tokio::time::interval(Duration::from_millis(250)); // spinner/AI progress
loop {
    terminal.draw(|f| render(f, &app))?;
    tokio::select! {
        biased;                                // keys beat ticks; prevents tick starvation
        Some(Ok(ev)) = events.next() => handle_event(ev),
        Some(msg) = lsp_rx.recv() => app.on_lsp(msg),   // LSP/AI worker results
        _ = tick.tick() => app.on_tick(),
    }
}
```

- Always filter `key.kind == KeyEventKind::Press` — Windows emits Release events (double-trigger).
- Resize: `Terminal::draw` runs `autoresize` every render pass (verified in
  `ratatui-core/src/terminal.rs` pipeline docs) and full-redraws on size change. No explicit
  `resize()` call needed; just treat `Event::Resize` as "redraw next iteration".
- Rendering is buffer-diffed; re-render the whole UI every frame. Cache expensive *data*
  (styled diff `Line`s) in app state, rebuild only when the underlying data changes.

## 3. Layout proposal

```text
┌──────────────────────────────────────────────────────────────────────┐
│ codescope │ repo ▸ branch ◂ base │ scope: branch │ gopls ✓ │ AI: on │  <- Length(1)
├─────────────┬──────────────────────────────────────────┬─────────────┤
│ changed     │  focused diff (selected file/symbol)     │  semantic   │
│ files +     │  + green adds / - red dels / @@ hunks    │  callers ▸  │
│ symbols     │                                          │  callees ▸  │
│ (List)      │                                          │  impact tree│
├─────────────┴──────────────────────────────────────────┴─────────────┤
│ Q quit · ? help · Tab pane · s scope · a AI │ msg: ...               │  <- Length(1)
└──────────────────────────────────────────────────────────────────────┘
```

Concrete constraints:

```rust
let outer = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]);
// wide (w >= 120): side panes fixed, diff absorbs slack (diffs benefit most from width)
let main = Layout::horizontal([Constraint::Length(30), Constraint::Min(40), Constraint::Length(36)]);
```

Responsive tiers (compute from `frame.area()` at render time — no stored layout state):

| tier | condition | panes |
|---|---|---|
| wide | w ≥ 120, h ≥ 20 | files 30 + diff min(40) + semantic 36 |
| medium | 80 ≤ w < 120 | files 26 + diff min(30); semantic as toggle overlay on `3`/`i` |
| narrow | w < 80 or h < 15 | single pane; `Tab` cycles Files → Diff → Semantic |
| too small | w < 30 or h < 8 | centered "terminal too small (WxH)" message only |

Notes: ratatui's cassowary solver never panics on over-constrained layouts; it silently
adjusts. Side panes use `Length` (predictable), center uses `Min` (slack + shrink target).
Help bar may drop out below h = 12 (recompute outer constraints per tier). Popups (help
modal): center via `Layout::vertical/horizontal([...]).flex(Flex::Center)` + `widgets::Clear`.

## 4. Keymap proposal

No modes — codescope is read-only, so a single "normal" mode with **both** vim keys and
arrows. Map keys through one pure function `map_key(ctx, KeyEvent) -> Option<Action>` so the
whole keymap is unit-testable without a terminal.

| key(s) | action |
|---|---|
| `Q`, `Ctrl-C` | quit |
| `?` | toggle help modal (lists this table) |
| `Tab` / `Shift-Tab`, `1` `2` `3` | cycle / directly focus files, diff, semantic pane |
| `s` / `S` | cycle comparison scope forward / backward |
| `R` | re-scan git data |
| `j`/`k`, `↓`/`↑` | move selection / scroll focused pane |
| `Enter` | files: jump diff+semantic to symbol; semantic: re-center impact graph on symbol |
| `Space`, `h`/`l`, `←`/`→` | collapse/expand tree node (files, impact) |
| `+` / `-` | increase / decrease default semantic expansion depth |
| `Ctrl-d`/`Ctrl-u`, `PgDn`/`PgUp` | half-page / page scroll in diff |
| `n` / `N` | next / previous diff hunk |
| `g` / `G`, `Home`/`End` | top / bottom |

Rationale: vim keys for power users, arrows + PgDn etc. for discoverability; help modal on
`?` is the primary discovery path. Avoid `:`-commands and insert mode (nothing to type).

## 5. Rendering the visualization model

- **Diffs**: normalize to `Vec<DiffRow { kind: Add|Del|Context|HunkHeader, old_ln: Option<u32>, new_ln: Option<u32>, text: String }>` when data changes; map to `Line`s at render (or cache styled `Line`s). Colors: `Color::Green` adds, `Color::Red` dels, `DarkGray` context + line numbers, `Cyan` hunk headers. Use named colors, not RGB — they track the user's terminal theme. Selected line: `Modifier::REVERSED` (theme-safe) rather than a hardcoded bg.
- **Diff widget**: `Paragraph` + `.scroll((y, x))` (supports both axes; diff lines are long) with a small `{y, x}` scroll struct clamped to content height, plus `Scrollbar`/`ScrollbarState` on the right edge. `List` is wrong here (no horizontal scroll).
- **Trees**: `tui-tree-widget 0.24.1` works with ratatui 0.30 (verified rendering in a test): `Tree::new(&items)` + identifier-based `TreeState` (open/close tracked by stable ids — use symbol paths as ids). Good for the impact graph. For the simpler files+symbols pane, either the same widget or a hand-rolled `List` with guide glyphs `│ `, `├─ `, `╰─ ` from `symbols::line` — hand-rolled is easier to assert in tests.
- **Focus state**: focused pane gets `Style::new().fg(Color::Cyan)` border + title with key hint; unfocused `DarkGray` border. Keep all panes mounted; only style changes (cheap, no layout thrash).
- **Scrolling state**: prefer stateful widgets (`ListState`, `TreeState`, `ScrollbarState`) over manual indices — they handle clamping.

## 6. Headless testing (verified patterns)

- `TestBackend::new(w, h)` + `Terminal::new(backend)` + `terminal.draw(|f| render(f, &app))`.
  Then either:
  - string check: `terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>()` + `contains(...)` (verified working), or
  - exact snapshot: `backend().assert_buffer_lines([...])` / `assert_buffer(&Buffer)` (both exist in 0.30, verified in `ratatui-core/src/backend/test.rs`).
- `TestBackend` implements `Display` → optional `insta` snapshots: `insta::assert_snapshot!(terminal.backend())`.
- Synthetic input: `KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)` is `const` and defaults to `kind: Press` (verified in crossterm source) — feed straight into `App::on_key`.
- Three test layers:
  1. pure: `map_key` + `App::apply(action)` state transitions (no ratatui types),
  2. render smoke: draw at 120×30, 80×24, 40×10, 20×5 — must not panic, key content present,
  3. sweep: draw at every width 0..=160 (few heights) — catches u16 underflow in custom Rect math. Use `Rect::inner(Margin)` (saturating) instead of manual subtraction.
- Async loop itself: keep the loop thin; all logic in sync handlers so tests never need tokio.

## 7. Pitfalls checklist

1. crossterm version must be 0.29.x to match ratatui 0.30's default backend (else duplicate crossterm, type mismatch).
2. Missing `KeyEventKind::Press` filter → double keys on Windows.
3. Custom panic/color-eyre hook must be installed *before* `ratatui::init()`.
4. `tokio::select!` without `biased;` lets a fast tick starve key handling.
5. Don't build styled diff `Line`s every frame — cache on data change.
6. Avoid `Frame::size()` (deprecated in 0.30; use `frame.area()`).
7. Don't index-split results by hand in narrow tiers — compute the constraint vector per tier so `split` length always matches.
8. Hardcoded RGB colors + selection backgrounds break on light terminal themes; use named colors + `REVERSED`.

## 8. Recommended decisions

1. **Deps**: `ratatui = "0.30"`, `crossterm = { version = "0.29", features = ["event-stream"] }`, `tokio = { version = "1", features = ["rt-multi-thread","macros","sync","time"] }`, `futures = "0.3"`, `tui-tree-widget = "0.24"` (impact tree only). Skip `tui-textarea`, `ratatui-image`, spinner crates.
2. **Init**: `ratatui::init()`/`restore()`; rely on its built-in panic-restore hook; add `color-eyre` later *before* init if nicer panics are wanted.
3. **Loop**: single tokio task, `EventStream` + `biased select!` over keys / LSP+AI channels / 250 ms tick; redraw every iteration.
4. **Layout**: 4 responsive tiers (wide/medium/narrow/too-small), `Length` side panes + `Min(40)` diff, computed fresh from `frame.area()` each frame.
5. **Keymap**: modeless, vim + arrows, single pure `map_key` → `Action` function; `?` help modal.
6. **Rendering**: named-color diff lines cached per data change; `Paragraph::scroll` for diff; `tui-tree-widget` for impact; hand-rolled glyph-guides `List` for files+symbols; `REVERSED` for selection, cyan border for focus.
7. **Testing**: three layers (pure actions, fixed-size TestBackend smoke, width-sweep no-panic); `insta` snapshots optional via TestBackend's `Display`.
