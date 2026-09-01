# AI data flow and the validation boundary

codescope keeps **facts** and **interpretation** strictly separate. Git, the language server,
and source analysis produce facts. The AI *selects and arranges* facts into a clear view, and
may interpret the exact changed hunks into conceptual entityless nodes and links — always
visibly marked inferred. Typed entity and graph claims remain deterministic and fact-validated.

## What the AI never does

- invent repository entities — every cited file, symbol, range, or hunk must resolve in the
  fact store — or present an interpreted link as a graph-verified fact
- get a shell, the filesystem, or the network
- write to the repository
- replace the deterministic visualization with unverified content

The app is fully functional with AI disabled, unavailable, slow, or rate-limited.

## The pipeline

1. **Detect** — by default the user presses `R` to refresh repository state. With the
   TUI-only `--watch` flag, working-tree and git-dir watchers feed one coalescer. After a
   quiet window, one repository fingerprint decides whether anything actually changed;
   only then does the dispatcher bump the repo-state **epoch** once.
2. **Refresh** — git re-reads the change-set, then each changed file enters a bounded,
   asynchronous symbol-analysis queue. The focused file runs immediately and at most one
   background file warms alongside it. Expansion only controls whether completed symbols
   are visible.
3. **Deterministic first** — the TUI immediately renders the deterministic impact view.
4. **Digest** — a compact description (changed symbols, diagnostics, hunk summaries, 1-hop
   relationships, repo sketch; ~4–8k tokens, hard-capped) is assembled. The whole repository is
   never sent; absolute paths are stripped.
5. **Schedule + request** — once a row's symbol inventory (and, for symbols, its bounded
   caller/callee lookup) is complete, it becomes eligible for automatic generation. A central
   coordinator continuously reprioritizes the focused row first, then sibling symbols in the
   focused or expanded files, then one file-level summary for every other changed file.
   Untouched/collapsed symbols are deferred. Normal concurrency targets 4 requests; interactive
   work may burst to 12, with 64 as an absolute safety ceiling. At the burst boundary a newly
   focused row cancels the oldest lower-priority overflow request FIFO, and still-valid cancelled
   work returns to the queue tail. Focus changes also reclassify active work, so an old selection
   cannot retain focused protection. The headless backend disables all prefetch and requests only
   its explicit selection. Each job returns a reviewer-first `VisualizationPlan` via a
   single required
   `submit_visualization_plan` tool call (tool choice required by default;
   `CODESCOPE_AI_TOOL_CHOICE=auto` supports providers that reject forced tool calls).
   `title`, `intent`, `review_focus`, and `evidence` are all required, and the primary form
   must be structural — one of changed-symbol tree, call tree, type/impl tree, relationship
   flow, before/after, or sequence; the legacy `impact_summary` and `focused_diff` forms
   are not accepted as AI plans. Alongside the digest, the request proactively carries a
   selection-scoped packet of the exact changed lines: at most 8 hunks, balanced head/tail
   selection, fair totals of 160 lines / 20 KB. Every packet row carries copyable,
   one-based `[old:… new:…]` coordinates, so the model never has to calculate source lines.
   Read-only tools are advertised only when
   an executor can serve them; the dispatcher currently wires none (TUI and debug-ai
   alike), so everything the AI can cite comes from the request itself. A completed,
   validated plan is cached by stable file/symbol identity. After that file changes, its
   old plan is sent as an explicitly untrusted design seed: incremental revisions preserve
   useful structure, while substantial behavioral/topological changes rebuild it. The old
   plan is never evidence and never bypasses current-epoch validation. Exact-epoch results are
   cached per row, so navigating back is instant. The client also enforces a 10-request/minute
   token bucket (burst 10); scheduled jobs wait asynchronously for capacity instead of turning
   normal pacing into a false provider failure. Three consecutive provider failures pause only
   background warming—focused requests remain eligible.
6. **Validate** — every cited entity must resolve against the fact store (file exists,
   symbol resolves, hunk index valid), and a typed edge between two fact-backed entities must
   exist in the impact graph. Plan schema v4 also requires every AI node to carry one or two
   exact `code_refs` (`file`, zero-based hunk, old/new side, inclusive one-based lines). Node refs must stay in the selected diff; cross-file context remains plan-level evidence. Every
   referenced line is checked against the actual parsed hunk before it can drive highlighting.
   If `review_focus` fences an external or out-of-diff outcome, parse-time validation also requires
   `title` to begin `Implemented change:` and `intent` to begin `Implemented behavior:`; both must
   stop before that unshown handoff.
   Entityless conceptual nodes and their hunk-derived links need no entity check but always
   render inferred. Hallucinated entities are dropped (tree forms) or
   the form is rejected (flow/sequence). The AI-facing caps are nonempty 1–2 forms, ≤5
   nodes per form (4 is the default; core keeps larger internal backstops), ≤8 edges per
   form, and 1–4 evidence entries. A rejected plan gets up to 3 bounded repair turns —
   schema, entity/fact, and structural errors each receive targeted guidance — before the
   deterministic fallback. A sequence missing an ordered link is repaired; extra, back, or
   duplicate sequence edges are sanitized to one consecutive edge per step. An accepted
   validation report travels with its plan: `debug-ai` prints the full report (verdict,
   dropped items, notes), while the TUI shows one concise sanitized-plan warning above the
   diagram for ValidWithDrops.
7. **Epoch gate** — the plan carries the epoch it was requested against. If the repository has
   changed since, the plan is **stale**: the last valid render stays on screen with a "stale"
   badge and a new request is issued. A stale plan can never replace a newer state's view.
8. **Render + interact** — the TUI draws validated plans as width-aware compact diagrams:
   connected boxes when space permits, numbered ladders and compact trees when it does not,
   and target-labeled adjacency rows for nonlinear flows. Each node renders once and every edge
   names its effect/destination. Validator-verified connectors are solid; inferred or
   hunk-derived links are dashed and visibly labeled. The layout retains semantic node hitboxes
   from the same physical spans that were rendered. Mouse motion only redraws when the hovered
   target changes; it never starts AI/LSP work. Hover adds a non-colour diff-row cue to every
   exact linked logical row (including all wrapped fragments), while click/`Space` pins the node and expands a
   bounded detail strip with source locators. External assumptions get an upfront warning plus
   the full Review block, and plan-level evidence remains below the map.

## Headless debugging

`codescope debug-ai` runs this same dispatcher pipeline without initializing Ratatui. The
dispatcher publishes `UiSnapshot` through a shared `BackendOutput` abstraction: a watch-channel
implementation feeds the interactive TUI, while an mpsc implementation feeds the debug command.
The command selects a changed file/function using normal `Action`s and prints the validated plan
and its full validation report from `UiSnapshot.semantic.plan` / `.report`, so it cannot
silently drift into a second prompt or validator. Its dispatcher uses the focused-only scheduler
policy, so a one-shot debug run never spends requests on sibling symbols or other files.
Serialized plans omit fields that only carry defaults (summary, change badge, hints), keeping
the printed JSON compact. Per-node `code_refs` and optional `expanded_detail` remain visible in
`debug-ai`, which makes live prompt/schema inspection possible without starting a terminal.

## Failure modes

- **No API key** → AI status "off"; deterministic views only.
- **Timeout / rate limit** — configurable per-request timeout, `Retry-After` honored,
  exponential backoff, and local token-bucket admission awaited asynchronously; a circuit
  breaker (3 transport failures in 60 s) cools down for 60 s.
- **Malformed / hallucinated plan** — validation drops or rejects; deterministic fallback shows;
  the status line notes it.
- **Rapid edits** — changes are coalesced. The selected file's symbols are invalidated and
  reloaded first, then its plan regenerates; the AI is never asked on every keystroke.

## Provider neutrality

The client speaks OpenAI-compatible `chat/completions` with tool calling. It sends
`tool_choice: required` by default; set `CODESCOPE_AI_TOOL_CHOICE=auto` for providers that only
accept automatic tool selection. If an auto-mode model answers in plain text, the service
uses one of its bounded repair turns to request the required structured tool call. Verified against Prime
Inference (`https://api.pinference.ai/api/v1`) and OpenAI; anything compatible (Ollama, vLLM, …)
works by setting the base URL. The default model is a small, schema-constrained one — plans are
structured, so they don't need a frontier model.

## Privacy

- API keys come from **environment variables only** (`PRIME_API_KEY` > `OPENAI_API_KEY` >
  `ANTHROPIC_API_KEY`; first found wins, provider inferred from the key); a literal key in a
  config file is a hard error.
- Keys are wrapped in `secrecy::SecretString`; never logged, never shown.
- What leaves the machine is the digest (repo-relative paths, symbol names, hunk summaries),
  the selection-scoped packet of exact changed lines for the current selection (≤8 hunks,
  160 lines / 20 KB), and, after a regeneration, that selection's prior validated plan as a
  revision seed. All paths in both current and cached material are repo-relative. Read-only
  file-body tools are advertised only when an executor can serve them; the dispatcher
  currently wires none, so no file body is fetched on demand.
- The status bar always shows whether AI is on, loading, ready, stale, or failed.
