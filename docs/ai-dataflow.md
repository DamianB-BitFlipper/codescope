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
- publish unfinished draft boxes as though they were a completed summary

Interactive startup requires a configured AI provider. Slow requests show progress, and failures
remain explicit rather than being replaced by deterministic summary content.

## The pipeline

1. **Detect** — by default the user presses `g` to refresh repository state. With the
   TUI-only `--watch` flag, working-tree and git-dir watchers feed one coalescer. After a
   quiet window, one repository fingerprint decides whether anything actually changed;
   only then does the dispatcher bump the repo-state **epoch** once.
2. **Refresh** — git re-reads the change-set, then each changed file enters a bounded,
   asynchronous symbol-analysis queue. The focused file runs immediately and at most one
   background file warms alongside it. Expansion only controls whether completed symbols
   are visible.
3. **Facts first** — the relationship half immediately renders deterministic selected-change,
   caller, and downstream facts. The generated half renders only `AI in progress` until a final
   summary has passed validation.
4. **Research brief** — the initial request contains only the selection kind/target, virtual
   working directory, comparison scope, changed-file count, and the review assignment. It does
   not proactively include source bodies, hunks, diagnostics, symbols, or relationship lists.
   Absolute paths are stripped.
5. **Schedule + request** — manual generation is the default: `a` requests the current directory,
   file, or symbol, and `A` toggles automatic selection-following generation for the session.
   Automatic selection changes use a 250 ms debounce. File/symbol requests wait for that file's
   symbol inventory; directory requests can proceed from the changed-file/hunk facts already
   available and include only files below the selected directory. There is no prompt prefetch,
   sibling warming, or file-summary queue. Moving away cancels only an unsent request: a provider
   request that has started keeps running and caches its result under the original selection. The
   coordinator retains at most 16 active requests; a
   17th request aborts the oldest active generation and takes its place. Aborted work is not
   requeued. The TUI and headless backend use this same selection-only policy. Each job returns
   a reviewer-first `DiagramDraft` through a bounded agentic loop (at most 128 total research and
   diagram operations). `edit_visualization` applies atomic intent/form/node/edge/evidence
   create-update-delete commands, and `inspect_visualization` returns the current draft. The model
   never needs to resend the complete plan after each correction. Production research first
   requires `list_directory` for a directory selection or `git_status_file` for a file/symbol;
   this exposes the inventory before the model chooses an exact diff hunk. Later research and normal
   full-schema turns use `Auto` tool choice. After an exact diff is retained, the initial
   intent/form bootstrap, and a focused recovery after a provider-truncated response, use
   `Required` tool choice with one controller-selected canonical editor branch; the next normal
   turn returns to full-schema `Auto`. Ending a full `Auto` turn without another tool call
   requests validation and publication. The controller also validates after a bounded number of
   complete-draft refinements and before accepting an unsolicited delete/rebuild cycle.
   `intent`, `forms`, and `evidence` are required, and the primary form
   must be structural — one of changed-symbol tree, call tree, type/impl tree, relationship
   flow, before/after, or sequence; the legacy `impact_summary` and `focused_diff` forms
   are not accepted as AI plans. The dispatcher exposes a selection-scoped mini-shell:
   `list_directory`, `read_file`, `search_changed_files`, `git_status_file`, `git_diff_file`,
   plus capability-discovered `inspect_language_server` when semantic support is available.
   Directory paths are relative to a virtual cwd (the selected directory or
   selected file's parent); file tools additionally accept an exact repo-relative path or an unambiguous
   repo-path suffix. Absolute/parent-traversal paths are rejected, file reads cannot leave the
   selected changed files, and results are line/byte capped. Git status/diff results come from
   the immutable captured `ChangeSet`, so exact evidence cannot drift during the loop. Diff rows
   carry copyable one-based `[old:… new:…]` coordinates. A research-required plan cannot complete
   until a nonempty exact `git_diff_file` result is retained; status, source, or LSP reads alone are
   insufficient. Each accepted draft edit is retained
   in the shared snapshot/controller state, while the in-flight TUI view shows the
   complete, vertically scrollable tool-call lifecycle (`running`, `succeeded`, or `failed`)
   rather than unfinished boxes. Failed rows include a bounded, scrubbed error reason.
   Final diagram publication still requires full validation. No shell command is executed and no
   repository state is mutated.
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
   symbol resolves, hunk index valid). In a `relationship_flow`, `calls`, `imports`,
   `implements`, and `contains` between fact-backed entities must exist in the impact graph.
   `reads` and `writes` have no v0 graph counterpart, so they may remain presentational and are
   reported unverified rather than treated as proven relationships. Plan schema v6 also requires
   every AI node to carry one or two exact `code_refs` (`file`, zero-based hunk, old/new side,
   inclusive one-based lines). Node refs must stay in the selected research scope: one file for
   file/symbol selection, or any changed file below a directory selection. Every referenced line
   is checked against the actual parsed hunk, and every node must include at least one actual
   added/new-side or removed/old-side row rather than only unchanged context. Entityless
   conceptual nodes and their hunk-derived links need no entity check but always render inferred.
   Schema v6 adds `flows_to`: a renderer-native chronological/control-flow transition, not a
   graph fact. It is valid only in sequence forms and renders inferred/dashed, bypassing
   impact-graph lookup; any semantic Sequence edge still requires graph proof. Hallucinated entities are dropped
   (tree forms) or the form is rejected (flow/sequence). The AI-facing caps are nonempty 1–2 forms, ≤5
   nodes per form (4 is the default; core keeps larger internal backstops), ≤8 edges per
   form, and 1–4 evidence entries. A rejected plan gets up to 3 bounded repair turns —
   schema, entity/fact, and structural errors each receive targeted guidance — before the
   generated pane reports `AI failed`. It never substitutes deterministic relationships for a
   failed generated result. A sequence missing an ordered link is repaired; extra, back, or
   duplicate sequence edges are sanitized to one consecutive edge per step. An accepted
   validation report travels with its plan: `debug-ai` prints the full report (verdict,
   dropped items, notes), while the TUI shows one concise sanitized-plan warning above the
   diagram for ValidWithDrops.
7. **Epoch gate** — the plan carries the epoch it was requested against. If the repository has
   changed since, the plan is **stale**: the generated half returns to `AI in progress` and a new
   request is issued. A stale plan can never remain visible against a newer repository state.
8. **Render + interact** — the TUI draws validated plans as positioned boxes joined by directed
   relationship paths. A pure Canvas is the single source of current node rectangles, compact edge
   labels, routed paths, drawing order, and hit regions for both rendering and mouse input. It seeds
   responsive one- or two-column positions, clamps X to the live pane, and lets Y grow through one
   vertical scroll axis. The model never supplies coordinates. Validator-verified paths are solid;
   inferred or hunk-derived paths are dashed with hollow arrowheads. Optional edge descriptions are
   truncated to their available route lane. Mouse motion only redraws when the hovered target
   changes and never starts AI/LSP work. Hover adds a non-colour diff-row cue to every exact linked
   logical row, including wrapped fragments. Click/`Space` grows the selected box at the same X/Y
   and shows its lossless concise detail, expanded detail, and source refs inside the box. Dragging
   stores that box's session-local X/Y and reroutes its incident arrows from the new bounds.
   Clicking an arrow or compact label toggles its complete text in a top-layer overlay; long text
   pages under the mouse wheel. Opening, paging, and closing that overlay do not change any base
   node rectangle, edge path, canvas extent, or scroll position. Plan intent, sanitizer warnings,
   and evidence share the same canvas coordinate system, so geometry and rendering cannot drift.

## One diagram API, two agent surfaces

`codescope-core::DiagramCommand` is the common mutation contract. Internal inference receives it
as `edit_visualization`; an external coding agent sends the identical tagged JSON through
`codescope agent . diagram edit '<json>'`. Both edit the dispatcher-owned `DiagramDraft`, and
both finish through the same parser, epoch gate, fact validator, and renderer. The controller can
also use `diagram inspect`, `diagram schema`, and `diagram finish`; each edit waits for its
dispatcher acknowledgement and returns the resulting draft. A controller edit cancels only an
older internal writer for that same selection so two agents cannot race one draft; ordinary tree
navigation still leaves started requests running under the 16-entry queue policy.

## Headless debugging

`codescope debug-ai` runs this same dispatcher pipeline without initializing Ratatui. The
dispatcher publishes `UiSnapshot` through a shared `BackendOutput` abstraction: a watch-channel
implementation feeds the interactive TUI, while an mpsc implementation feeds the debug command.
The command selects a changed file/function and explicitly triggers generation using normal
`Action`s, then prints the validated plan
and its full validation report from `UiSnapshot.semantic.plan` / `.report`, so it cannot
silently drift into a second prompt or validator. A one-shot debug run requests only that
explicit selection.
Serialized plans omit fields that only carry defaults (summary, change badge, hints), keeping
the printed JSON compact. Per-node `code_refs` and optional `expanded_detail` remain visible in
`debug-ai`, which makes live prompt/schema inspection possible without starting a terminal.

## Failure modes

- **No API key** — interactive startup exits with a configuration error.
- **Timeout / rate limit** — configurable per-request timeout, `Retry-After` honored,
  exponential backoff, an 8-request in-flight cap, and the high 600 rpm local safety ceiling
  awaited asynchronously; a circuit breaker (3 transport failures in 60 s) cools down for 60 s.
- **Malformed / hallucinated plan** — validation drops invalid pieces or rejects the result after
  bounded repair; a terminal rejection shows the clickable `AI failed` banner.
- **Rapid edits** — changes are coalesced. The selected file's symbols are invalidated and
  reloaded first, then its plan regenerates; the AI is never asked on every keystroke.

## Provider neutrality

The client speaks OpenAI-compatible Chat Completions for OpenAI, Prime, and custom endpoints,
and native Anthropic Messages for Anthropic. A selection-specific initial inventory, the initial
intent/form bootstrap after retained diff evidence,
and a focused recovery from a provider-truncated response, use `Required` with one
controller-selected canonical branch. Other research/construction turns use `Auto`. A tool-less
full `Auto` turn is one completion signal; bounded finalization is another. If the accumulated
draft is invalid, the service uses one of its bounded repair turns to return exact validation
feedback. Verified against Prime
Inference (`https://api.pinference.ai/api/v1`) and OpenAI; anything compatible (Ollama, vLLM, …)
works by setting the base URL. The default model is a small, schema-constrained one — plans are
structured, so they don't need a frontier model.

## Privacy

- Key values come from **environment variables only**. A global `[ai].api_key_env` names the
  variable resolved first; otherwise resolution tries `PRIME_API_KEY`, `OPENAI_API_KEY`, then
  `ANTHROPIC_API_KEY` and infers the provider from the built-in name. An arbitrary named key
  requires an explicit base URL; a literal key in a config file is a hard error.
- Keys are wrapped in `secrecy::SecretString`; never logged, never shown.
- Every provider request, response, usage record, latency, and error is appended to that process's
  JSONL file under the local `telemetry/` directory beside global config. Prompt, tool, completion,
  and provider-returned reasoning content is retained after recognizable secret values are
  scrubbed. Headers and key material are excluded, and Codescope does not upload the file.
- After the dispatcher accepts a parsed comparison, a deduplicated `diff.snapshot` stores its
  complete privacy-filtered unified patch, resolved scope/base/head, and file/hunk byte mappings.
  Its SHA-256 `diff_snapshot_id` hashes the exact stored payload and correlates later UI,
  controller, snapshot, and LLM events. Refresh invalidation removes the active ID immediately;
  an in-flight LLM request keeps the ID with which it began.
- The same local stream correlates LLM turns with raw keys, typed picker input, selections,
  focused files/hunks, scroll offsets, mouse coordinates/gestures, external control actions, and
  snapshot transitions from the TUI session.
- What leaves initially is the bounded research assignment. Later fresh handoffs can include
  retained exact diffs and tagged tool results, the current diagram draft, bounded controller
  feedback/state, and an explicitly untrusted prior validated revision seed. These values use
  repo-relative paths, have absolute roots removed, and pass through secret scrubbing.
- The status bar shows the active AI service state. The changed tree independently marks each
  directory/file/symbol summary as `◆` ready, `◇` not generated, `◌` generating, or `!` failed.
