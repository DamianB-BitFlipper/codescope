# codescope — architecture (v0 prototype)

Lead-engineer synthesis of docs/research/*.md. Decisions are final for the prototype unless a
review finding forces a change.

## Workspace layout

Cargo workspace, 8 crates. Ownership boundaries are crate boundaries; no cross-crate edits by
implementation agents other than their own.

```
codescope/
├── Cargo.toml                 # workspace
├── crates/
│   ├── codescope-core/        # shared domain types, no async, no io
│   ├── codescope-git/         # read-only git CLI subprocess layer
│   ├── codescope-lsp/         # generic LSP client + gopls adapter
│   ├── codescope-analysis/    # change→symbol mapping, impact graph, digest
│   ├── codescope-ai/          # provider client, plan schema, validation
│   ├── codescope-tui/         # ratatui app: state, actions, render
│   ├── codescope/             # binary: config, wiring, dispatcher
│   └── codescope-testutil/    # go-fixture generator + fakes (dev-facing lib)
└── tests/                     # workspace-level integration tests
```

Dependency direction: core ← {git, lsp} ← analysis ← {ai, tui} ← codescope. testutil ← tests.
`tui` never calls `git`/`lsp`/`ai` directly — it renders `UiSnapshot` and sends `Action`s.

## Key decisions (with why)

1. **Git via CLI subprocess**, `--porcelain=v2 -z`, `--no-optional-locks` everywhere.
   Why: exact user-git semantics, no C build. (research 02)
2. **Enum-dispatch LanguageService** over one generic `LspClient`; gopls adapter only.
   Capabilities resolved at initialize into a `FeatureSet`; utf-16 default position encoding,
   converted at wire boundary only. Why: verified cross-server quirks (research 01).
3. **Hunks → symbols via hierarchical DocumentSymbols**; worktree tree for new-side hunks,
   base-revision overlay for pure deletions; confidence Exact/Approximate/Unmapped end-to-end.
   (research 03)
4. **Epoch supersede**: single dispatcher bumps the epoch per accepted change-set. Git reads,
   analysis, and AI requests run as spawned, epoch-tagged jobs; results are applied only when
   the epoch still matches (`on_analysis_done` / `on_ai_done`). Startup is non-blocking — the
   TUI renders a git-only view immediately and the language server is handed over via
   `EngineReady` when its initialize completes. Why: a stale AI plan or analysis must never
   overwrite a newer repo state, and a slow subsystem must never freeze the UI. (research 06)

   **Lazy per-file semantics (TUI)**: the interactive path analyzes a changed file only when
   the user expands it with `Tab` (`analyze_changed_file`); the files pane lists git changes
   immediately and each row carries a `FileSemanticLoad` state (Unloaded/Loading/Ready/
   Unsupported/Failed). Symbol relation queries are gated on the file's Ready state. The
   non-interactive backend (`analyze`/`digest`) stays eager via `refresh_with_ctx`.
5. **Channel topology**: per-subsystem unbounded mpsc → one dispatcher actor →
   `watch::channel<UiSnapshot>` → TUI `select!{biased;}` over crossterm EventStream + tick.
6. **AI**: OpenAI-compatible chat completions via reqwest 0.13; plan returned through a single
   required `submit_visualization_plan` tool call; 8-form enum; full fact-validation boundary
   (epoch gate, entity resolution, edge existence, hunks by reference); deterministic fallback
   always. AI off unless configured. (research 05)
7. **Privacy**: 4-layer exclusion (git ignore rules < .codescopeignore < compiled denylist <
   content sniffing), applied to diff paths too; keys via env name only into secrecy::SecretString.
   (research 07)
8. **TUI**: ratatui 0.30 + crossterm 0.29 (pin both; mismatch = duplicate crossterm);
   `ratatui::init/restore` (panic-safe), 4-tier responsive layout, modeless keymap, pure
   `map_key` for testability. (research 04)
9. **Fixture + tests**: shell-regenerable Go fixture with deterministic OIDs; hand-rolled fake
   LSP server for negative tests; ScriptedProvider fake AI; live AI behind #[ignore] + env.
   (research 08)

## Data model anchors (codescope-core)

- `RepoContext { toplevel, head: HeadState, upstream, base }`
- `ChangeSet { scope: ChangeScope{Branch|Staged|Unstaged}, files: Vec<FileChange> }`,
  `FileChange{path, old_path, status, hunks, binary}`, `Hunk{old/new start/len, section}`
- `SymbolNode{id,name,detail,kind,range,selection,children}` / `SymbolTree{file,revision,roots}`
- `ChangedSymbol{symbol, change_kind, hunks, confidence}` / `MappingConfidence`
- `Evidence<T>{value, completeness, notes}` — honesty layer on all relationship queries
- `ImpactGraph { nodes: Vec<ImpactNode>, edges: Vec<ImpactEdge> }` with edge kinds
  calls/called_by/implements/implemented_by/references/contains
- `VisualizationPlan` (8 forms) + `ValidationReport`
- `UiSnapshot` — everything the TUI renders, immutable, watch-channel payload
- `Epoch(pub u64)` newtype over the repo-state generation

## Vertical slice order (each independently testable)

1. core types + git → ChangeSets (fixture-backed tests)
2. lsp client + gopls → symbols/references/call hierarchy (fixture-backed, skip w/o gopls)
3. analysis → ChangedSymbols + ImpactGraph (pure tests + fixture)
4. tui → renders UiSnapshot headless (TestBackend), keymap actions
5. ai → fake provider plan validation; optional live smoke
6. wiring → dispatcher, watchers, epoch flow (integration test on fixture copy)

## Explicit non-goals for v0

- tree-sitter fallback, runtime data flow, multi-language beyond Go, plan streaming renders,
  `$/cancelRequest` to gopls, writing anything to the repo.

## Lead decisions recorded during implementation

- schemars lives in codescope-ai (schema generated there from core plan types); core stays serde-only.
- UiSnapshot is owned by codescope-tui (renders it); binary crate assembles it from subsystems.
- core Epoch is u64; the AI plan "epoch" field carries its string form (hash/fingerprint).
