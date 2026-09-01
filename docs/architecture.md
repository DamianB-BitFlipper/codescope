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

   **Refresh policy (TUI)**: startup performs one initial refresh. Repository watching is
   opt-in with `--watch`; otherwise the repository changes only when the user presses `R`
   (scope and base selections also refresh because they are explicit comparison changes).
   Both manual and watcher events enter the same epoch-gated dispatcher path. Headless
   commands are one-shot and never start native watchers.

   **Asynchronous per-file semantics (TUI)**: the files pane lists git changes immediately,
   then every changed file enters a priority queue independent of expansion. One focused
   analysis may run beside one background warm-up; selection changes reprioritize queued
   work immediately. Each row carries a `FileSemanticLoad` state (Unloaded/Loading/Ready/
   Unsupported/Failed), and `Tab` controls visibility only. Symbol relation queries and
   automatic AI generation are gated on the selected file's Ready state. The
   language-server child is limited to two workers, and content-addressed symbol trees are
   reused across epochs. The non-interactive backend (`analyze`/`digest`) stays eager via
   `refresh_with_ctx`.
5. **Backend-output boundary**: per-subsystem mpsc → one dispatcher actor →
   `BackendOutput::publish(UiSnapshot)`. The interactive implementation publishes latest-value
   state through `watch::channel<UiSnapshot>` to the TUI; the headless implementation publishes
   ordered snapshots through mpsc to `debug-ai`. Both consumers therefore exercise the same
   snapshot assembly, actions, epoch/selection gates, AI request, and validation path. The TUI
   waits on that watch receiver or crossterm EventStream and redraws only after state/input
   changes; there is no idle frame tick.
   Mouse input uses the retained `UiGeometry` from the last rendered frame. Dividers,
   independently scrollable regions, and AI-node span hitboxes are registered there with stable
   identities and bounds. Wheel events update the hovered region without changing focus or
   selection. Any-motion events update only when the semantic node target changes; hover is local
   UI state and never starts backend work.
6. **AI**: OpenAI-compatible chat completions via reqwest 0.13; plan returned through a single
   required `submit_visualization_plan` tool call; six structural forms (legacy
   `impact_summary`/`focused_diff` are rejected at the AI plan boundary); reviewer-first
   contract (required title/intent/review_focus, 1–4 evidence, default 4 / hard max 5 nodes
   and ≤8 edges per form, nonempty 1–2 forms — core keeps larger node backstops). Plan schema v4
   requires every AI node to carry 1–2 exact old/new diff ranges and permits one optional expanded
   explanation. External-review plans must fence their title and intent with explicit implemented-only
   prefixes. The fact-validation boundary checks epoch, entity resolution, typed-edge existence,
   hunks, and every referenced source line, with up to 3 bounded repair turns; accepted validation reports travel with
   plans (debug-ai prints the full report; the TUI shows one sanitized-plan warning);
   deterministic fallback always. One request coordinator continuously reprioritizes focused
   selection, visible sibling symbols, then other file summaries. It admits 4 normal requests,
   permits interactive bursts to 12, and enforces an absolute ceiling of 64. At the burst
   boundary, focused work aborts the oldest lower-priority overflow request and returns that
   still-valid work to the queue tail. Completed plans cache by selection; provider admission
   also waits on a local token bucket, and repeated failures pause background prefetch while
   focused work remains eligible. The headless policy is focused-only.
   AI off unless configured. (research 05)
7. **Privacy**: 4-layer exclusion (git ignore rules < .codescopeignore < compiled denylist <
   content sniffing), applied to diff paths too; keys via env name only into secrecy::SecretString.
   (research 07)
8. **Global configuration**: v1 TOML at the XDG/platform user config path (override with
   `CODESCOPE_CONFIG`), with no repository-local layer. Environment variables override `[ai]`;
   explicit model-picker choices are remembered per provider. Only stable view preferences are
   persisted. A FIFO blocking worker patches known keys under a sibling lock and atomically
   replaces the file without stalling the dispatcher, while preserving comments/unknown v1 keys
   and never serializing secrets or repository state.
9. **TUI**: ratatui 0.30 + crossterm 0.29 (pin both; mismatch = duplicate crossterm);
   `ratatui::init/restore` (panic-safe), 4-tier responsive layout, modeless keymap, pure
   `map_key` for testability. Validated AI plans render as width-aware compact diagrams —
   connected boxes where space permits, numbered ladders, compact trees, and document-order
   target-labeled adjacency lines for nonlinear flows (each node once, edges naming effect and
   destination, cycles explicit). Dashed labeled connectors mark inferred/hunk-derived links.
   Hovering a node highlights its validated logical diff rows without erasing add/delete meaning;
   click/`Space` opens a bounded details/source strip. External assumptions remain above the full
   Review block. (research 04)
10. **Fixture + tests**: shell-regenerable Go fixture with deterministic OIDs; hand-rolled fake
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
- `VisualizationPlan` (six structural forms) + `ValidationReport`
- `UiSnapshot` — everything the TUI renders, immutable, watch-channel payload
- `Epoch(pub u64)` newtype over the repo-state generation

## Vertical slice order (each independently testable)

1. core types + git → ChangeSets (fixture-backed tests)
2. lsp client + gopls → symbols/references/call hierarchy (fixture-backed, skip w/o gopls)
3. analysis → ChangedSymbols + ImpactGraph (pure tests + fixture)
4. tui → renders UiSnapshot headless (TestBackend), keymap actions
5. ai → fake provider plan validation; optional live smoke
6. wiring → dispatcher, optional watchers, manual refresh, epoch flow (integration test on fixture copy)

## Explicit non-goals for v0

- tree-sitter fallback, runtime data flow, multi-language beyond Go, plan streaming renders,
  `$/cancelRequest` to gopls, writing anything to the repo.

## Lead decisions recorded during implementation

- schemars lives in codescope-ai (schema generated there from core plan types); core stays serde-only.
- UiSnapshot is owned by codescope-tui (renders it); binary crate assembles it from subsystems.
- core Epoch is u64; the AI plan "epoch" field echoes the same integer in JSON (e.g. 2).
