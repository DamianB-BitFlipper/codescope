# 05 — AI-Assisted Visualization Design

Scope: how codescope uses an LLM to *choose and parameterize* visualizations of the current
change. The app owns facts, validation, and rendering. The AI never invents entities and is
never required. Deterministic impact remains available outside the generated result, but a failed
AI view reports failure explicitly instead of impersonating a summary with fallback data.

Sources verified locally: HumanLayer Show Me SKILL.md (fetched 2026-08), crates.io versions,
`prime inference models` output, Prime Inference base URL from the prime-intellect skill docs.

## 1. Show Me skill — philosophy → rules for codescope

Read: `plugins/show-me/skills/show-me/SKILL.md` in `humanlayer/skills`. Its core ideas, and
the rule each becomes for codescope:

1. **"Pick the smallest view that makes the key point clear."**
   → Rule S1: a plan answers exactly one question. `focus` is a required field (one sentence).
2. **Match the form to the question type** (its decision table: algorithm→pseudocode,
   runtime flow→call tree, UI structure→component tree, file responsibilities→shallow tree,
   interaction→sequence/flow, "what changes"→diff, mostly-new→whole block).
   → Rule S2: the AI picks from a fixed form enum (§2); no free-form drawing.
3. **"Match the diff shape to the topic"** — it diffs *structures* (component tree, file tree,
   call tree, state machine), not just text.
   → Rule S3: before/after and diff forms operate on symbol/tree structures; text diffs are
   the last resort, not the default.
4. **"Keep only the calls, files, props, states, and boundaries needed to answer the current
   question."** → Rule S4: hard caps — default ≤12 nodes, depth ≤3, one form per plan
   (two max). The TUI offers "expand" interactions instead of bigger plans.
5. **"Skip the preamble… you may use one of these, unlikely all of them."**
   → Rule S5: one screen, one visual. No dashboards. Prose summary ≤3 lines, rendered by the
   TUI next to the visual (Show Me: "place each visual next to the short text it supports").
6. **It uses the real labels/paths of the project.** → Rule S6: every label comes from a
   resolved fact-store entity, never from AI text (enforced by §3).

## 2. VisualizationPlan schema (AI output, TUI-rendered)

JSON, produced via a tool call (§5). Serde-friendly; versioned.

```json
{
  "plan_version": 4,
  "epoch": 7,
  "focus": "How does the changed request path load a session?",
  "title": "Session loading now follows the guarded cache path",
  "intent": "The handler checks cached state before falling through to persistent storage.",
  "review_focus": "Confirm cache misses still preserve the typed storage error.",
  "forms": [{
    "kind": "relationship_flow",
    "title": "Request path",
    "nodes": [{
      "id": "n1",
      "entity": {
        "file": "src/session/store.rs",
        "symbol": "session::store::SessionStore::load"
      },
      "label": "SessionStore.load",
      "detail": "checks cached state before reading storage",
      "expanded_detail": "A cache miss continues through the existing typed storage result.",
      "code_refs": [{
        "file": "src/session/store.rs",
        "hunk": 0,
        "side": "new",
        "start_line": 122,
        "end_line": 126
      }]
    }, {
      "id": "n2",
      "label": "persistent lookup",
      "detail": "returns the loaded session or typed failure",
      "code_refs": [{
        "file": "src/session/store.rs",
        "hunk": 0,
        "side": "new",
        "start_line": 127,
        "end_line": 130
      }]
    }],
    "edges": [{
      "from": "n1", "to": "n2", "kind": "reads", "label": "on cache miss"
    }]
  }],
  "evidence": [{
    "file": "src/session/store.rs", "hunk": 0,
    "reason": "contains the guarded cache and storage path"
  }]
}
```

The AI-facing form enum is fixed: `changed_symbol_tree`, `call_tree`, `type_impl_tree`,
`relationship_flow`, `before_after`, and `sequence`. Legacy stored/internal
`impact_summary` and `focused_diff` values do not pass the AI validation boundary. Language-neutral
entities remain `(file, symbol, range)`. A node's `code_refs` are separate hover anchors into the
focused unified diff: file + zero-based hunk + explicit old/new side + inclusive one-based lines.

Pitfalls: (a) do not let the AI emit Mermaid/text art — TUI can't validate it; (b) `range`
must be optional for tree roots (file-level nodes: symbol omitted); (c) enforce caps at
validation, not in the prompt only.

## 3. Fact-validation boundary

Validation is local, deterministic, and has no AI in the loop. Pipeline:

1. **Epoch gate.** Epoch = hash of `(HEAD sha, index hash, mtime/size manifest of working
   tree, LSP snapshot version)`. Plan carries the epoch from its prompt. If current epoch
   differs: attempt cheap re-resolution of every entity (symbols by name+kind, ranges by
   content anchor); if any structure moved → mark plan *stale*, show last valid render with a
   "regenerating" badge, re-request. Never silently show a stale plan as fresh.
2. **Entity resolution.** Every `entity` must resolve to exactly one fact-store entry
   (file exists, symbol exists in that file's outline, range within the symbol's extent or
   equal to it). Unresolvable = hallucination.
3. **Edge validation.** `calls/implements/imports` edges must exist in the impact graph.
   The AI may *select* edges, not assert new ones. (If we later want hypothesis edges, add
   `"inferred": true`, render dashed; still require resolvable endpoints.)
4. **Hunk and code-link validation.** Plan evidence references `(file, hunk_index)`. Every
   AI node also carries one or two exact code ranges. The validator confirms the file/hunk and
   each one-based line on the declared old/new side against parsed diff rows before the range can
   drive hover highlighting. Reversed, oversized, missing, or wrong-side ranges trigger a bounded
   repair; the model never emits screen coordinates.

Hallucination policy (per form):
- **Tree forms** (`changed_symbol_tree`, `call_tree`, `type_impl_tree`, `before_after`):
  drop the invalid node, re-parent its children to the node's parent, record a warning.
  If >20% of nodes invalid or the root invalid → reject the form.
- **Flow/sequence forms**: invalid endpoint breaks ordering semantics → reject the form.
- **`impact_summary`**: drop invalid bullets; reject if empty.
- Any terminal rejection → one clickable `AI failed` banner with retained diagnostic detail.
  Log all drops/rejects; a "plan validation" debug pane helps tune prompts.

## 4. Compact change description (prompt payload) + tool surface

Goal: fit the *decision-relevant* change in ~4–8k tokens, hard cap 12k. Assemble in
priority order; truncate from the bottom of each tier, never drop tier 1–2:

1. **Changed symbols** (required): per symbol — fq name, kind, container, change kind,
   before/after signature if cheap, diagnostic count. Cap 50.
2. **Diagnostics** touching changed ranges: severity, code, message (truncated 160 chars). Cap 30.
3. **Hunk summaries** (not bodies): file, hunk header, +/− counts, first 2 lines of each
   side for context. Cap 40 hunks.
4. **1-hop relationships**: direct callers/callees/implementers of changed symbols, name-only.
   Cap 100 total; annotate which are themselves changed.
5. Repo map sketch: top-level dirs of changed files, 1 line each (Show Me's "shallow tree").

Read-only tool surface (OpenAI tool-calling format; all paths repo-relative, sandboxed to
repo root, all results capped, total budget ≤8 calls per plan):

| tool | args | returns (capped) |
|---|---|---|
| `get_file_outline` | `file` | symbols: name/kind/range/container (200) |
| `get_symbol` | `file`, `symbol` | signature, doc (20 lines), range, kind |
| `get_hunk` | `file`, `hunk_index` | full unified hunk text (200 lines) |
| `get_references` | `symbol`, `limit≤50` | ref sites: file/range/preview line |
| `get_callers` / `get_callees` | `symbol`, `depth≤2` | tree of fq names + edge kind |
| `get_implementations` | `symbol` | impls of interface/trait + file/range |
| `search_symbols` | `query`, `limit≤20` | fuzzy workspace symbol matches |
| `get_diagnostics` | `file?` | current diagnostics (50) |

Every tool result is generated from the fact store — so anything the AI later cites is
guaranteed resolvable (tool output includes the exact `entity` JSON to echo back).

## 5. Provider-neutral client

**Wire protocol: OpenAI chat completions + tool calling.** Verified compatible targets:
OpenAI (`https://api.openai.com/v1`) and Prime Inference (`https://api.pinference.ai/api/v1`,
models like `openai/gpt-5.4`, `openai/gpt-5-mini` — confirmed via `prime inference models`).
Also covers Ollama/vLLM/LM Studio for free.

Plan construction uses shared incremental diagram tools with automatic tool selection: atomic
edits mutate a bounded draft, inspection returns its current state, and a natural end to the tool
sequence requests deterministic validation/publication. There is no whole-plan tool fallback or
separate model completion tool.

**Crate choice: `reqwest` 0.13.4 + `serde` 1.0.229 + `serde_json` 1.0.151** (features:
`json`, `stream`, `rustls-tls`). One endpoint, ~150 lines of types; full control over the
exact payload and errors. `async-openai` 0.41.3 is a viable alternative (verified:
`OpenAIConfig::with_api_base()` / `with_api_key()` support custom endpoints), but it tracks
the whole OpenAI API surface — churn without payoff for one endpoint. Pitfall: streaming SSE —
`reqwest-eventsource` 0.6.0 pins `reqwest ^0.12`, so on reqwest 0.13 use `eventsource-stream`
0.2.3 directly over `response.bytes_stream()` and parse `data:` frames yourself ([DONE] sentinel).

HTTP streaming remains off. Accepted draft edits remain available through the shared draft API;
between ordinary tool turns the TUI shows each research/edit call progressing through
running/succeeded/failed. Diagram publication remains atomic after validation.

Config resolution remains env-first, but interactive startup now requires the result to be enabled:
- `CODESCOPE_AI=on` supports keyless local endpoints; an explicit `off` result prevents startup
- `CODESCOPE_AI_BASE_URL` (default `https://api.pinference.ai/api/v1` if `PRIME_API_KEY` set,
  else `https://api.openai.com/v1`), `CODESCOPE_AI_API_KEY` (fallback: `PRIME_API_KEY`, then
  `OPENAI_API_KEY`), `--model` / `-m` (default `openai/gpt-5-mini`-class; plans are
  schema-constrained so a small model suffices), `CODESCOPE_AI_TIMEOUT_MS` (default 20000),
  `CODESCOPE_AI_STREAM=0|1`. Wrap the key in `secrecy` 0.10.3; never log it.

Privacy: send only the §4 digest (repo-relative paths, symbols, hunk summaries). File bodies
leave the machine only if the AI explicitly calls `get_hunk`/`get_symbol`. Redact absolute
paths and env values from the payload.

## Recommended decisions

1. AI selects from the structural form enum and incrementally describes boxes and relationships;
   the TUI owns all placement/rendering and reports generation failure explicitly.
2. Adopt Show Me's rules as plan constraints: one focus question, ≤12 nodes, depth ≤3, ≤3-line
   summary, structural diffs over text diffs.
3. Validate everything: epoch gate, entity resolution, edge existence in the impact graph,
   hunks by reference. Drop invalid nodes in tree forms (>20% or bad root → reject), reject
   broken flow/sequence forms outright, fall back silently-ish with a status notice.
4. Prompt digest of 5 tiers, ~4–8k tokens, hard cap 12k; 8 read-only tools, ≤8 calls/plan;
   tool outputs carry ready-to-echo `entity` JSON.
5. Client: reqwest + serde, OpenAI-compatible chat completions and the shared incremental
   diagram tools; `CODESCOPE_AI_*` env config with `PRIME_API_KEY` auto-detection; interactive
   startup fails clearly when no provider credential or explicit local endpoint exists.
