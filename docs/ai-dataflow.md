# AI data flow and the validation boundary

codescope keeps **facts** and **interpretation** strictly separate. Git, the language server,
and source analysis produce facts. The AI *selects and arranges* facts into a clear view, and
may interpret the exact changed hunks into conceptual entityless nodes and links — always
visibly marked inferred. Typed entity and graph claims remain deterministic and fact-validated.

## What the AI never does

- invent repository entities — every cited file, symbol, range, or hunk must resolve in the
  fact store — or present an interpreted link as a graph-verified fact
- execute a shell command, access the network, or read outside the selected changed-file scope
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
4. **Research brief** — the initial request contains only the selection kind/target, virtual
   working directory, comparison scope, changed-file count, and the review assignment. It does
   not proactively include source bodies, hunks, diagnostics, symbols, or relationship lists.
   Absolute paths are stripped.
5. **Schedule + request** — only the current directory, file, or symbol can start automatic
   generation. Selection changes use a 250 ms debounce. File/symbol requests wait for that
   file's symbol inventory; directory requests can proceed from the changed-file/hunk facts
   already available and include only files below the selected directory. There is no prompt
   prefetch, background generation, sibling warming, or file-summary queue. Moving away cancels
   only the unsent debounce: a provider request that has started keeps running and caches its
   result under the original selection. The coordinator retains at most 16 active requests; a
   17th request aborts the oldest active generation and takes its place. Aborted work is not
   requeued. The TUI and headless backend use this same selection-only policy. Each job returns
   a reviewer-first `DiagramDraft` through a bounded agentic loop (at most 48 total research and
   diagram operations). `edit_visualization` applies atomic intent/form/node/edge/evidence
   create-update-delete commands, `inspect_visualization` returns the current draft, and
   `finish_visualization` asks the validator to publish it. The model never needs to resend the
   complete plan after each correction. Tool choice is required by default;
   `CODESCOPE_AI_TOOL_CHOICE=auto` supports providers that reject forced tool calls.
   `intent`, `forms`, and `evidence` are required, and the primary form
   must be structural — one of changed-symbol tree, call tree, type/impl tree, relationship
   flow, before/after, or sequence; the legacy `impact_summary` and `focused_diff` forms
   are not accepted as AI plans. The dispatcher exposes a selection-scoped mini-shell:
   `list_directory`, `read_file`, `search_changed_files`, `git_status_file`, and
   `git_diff_file`. Paths are relative to a virtual cwd (the selected directory or selected
   file's parent), absolute/parent-traversal paths are rejected, file reads cannot leave the
   selected changed files, and results are line/byte capped. Git status/diff results come from
   the immutable captured `ChangeSet`, so exact evidence cannot drift during the loop. Diff rows
   carry copyable one-based `[old:… new:…]` coordinates. A finish requested before one successful
   research call is rejected and the model is sent back to research. Each accepted draft edit can
   be published to the TUI immediately; only renderable projections are shown, and final publication
   still requires full validation. No shell command is executed and no repository state is mutated.
   A completed,
   validated plan is cached by stable directory/file/symbol identity. After that selection changes, its
   old plan is sent as an explicitly untrusted design seed: incremental revisions preserve
   useful structure, while substantial behavioral/topological changes rebuild it. The old
   plan is never evidence and never bypasses current-epoch validation. Exact-epoch results are
   cached per row, so navigating back is instant. Provider admission primarily uses an 8-permit
   in-flight semaphore. A 600-request/minute token bucket (burst 100) remains as a high safety
   ceiling; active jobs wait asynchronously for both capacities instead of turning normal pacing
   into a false provider failure.
6. **Validate** — every cited entity must resolve against the fact store (file exists,
   symbol resolves, hunk index valid), and a typed edge between two fact-backed entities must
   exist in the impact graph. Plan schema v5 also requires every AI node to carry one or two
   exact `code_refs` (`file`, zero-based hunk, old/new side, inclusive one-based lines). Node refs
   must stay in the selected research scope: one file for file/symbol selection, or any changed
   file below a directory selection. Every
   referenced line is checked against the actual parsed hunk before it can drive highlighting.
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

## One diagram API, two agent surfaces

`codescope-core::DiagramCommand` is the common mutation contract. Internal inference receives it
as `edit_visualization`; an external coding agent sends the identical tagged JSON through
`codescope agent . diagram apply '<json>'`. Both edit the dispatcher-owned `DiagramDraft`, and
both finish through the same parser, epoch gate, fact validator, and renderer. The controller can
also use `diagram show`, `diagram reset`, and `diagram finish`. A controller edit cancels only an
older internal writer for that same selection so two agents cannot race one draft; ordinary tree
navigation still leaves started requests running under the 16-entry queue policy.

## Headless debugging

`codescope debug-ai` runs this same dispatcher pipeline without initializing Ratatui. The
dispatcher publishes `UiSnapshot` through a shared `BackendOutput` abstraction: a watch-channel
implementation feeds the interactive TUI, while an mpsc implementation feeds the debug command.
The command selects a changed file/function using normal `Action`s and prints the validated plan
and its full validation report from `UiSnapshot.semantic.plan` / `.report`, so it cannot
silently drift into a second prompt or validator. A one-shot debug run requests only that
explicit selection.
Serialized plans omit fields that only carry defaults (summary, change badge, hints), keeping
the printed JSON compact. Per-node `code_refs` and optional `expanded_detail` remain visible in
`debug-ai`, which makes live prompt/schema inspection possible without starting a terminal.

## Failure modes

- **No API key** → AI status "off"; deterministic views only.
- **Timeout / rate limit** — configurable per-request timeout, `Retry-After` honored,
  exponential backoff, an 8-request in-flight cap, and the high 600 rpm local safety ceiling
  awaited asynchronously; a circuit breaker (3 transport failures in 60 s) cools down for 60 s.
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
- What leaves initially is the compact research brief. On later turns, only the bounded tool
  results the model requested leave the machine: selection-scoped changed-file sections, literal
  search matches, or captured per-file status/diff facts. Results and any prior validated revision
  seed use repo-relative paths, have absolute roots removed, and pass through secret scrubbing.
- The status bar shows the active AI service state. The changed tree independently marks each
  directory/file/symbol summary as `◆` ready, `◇` not generated, `◌` generating, or `!` failed.
