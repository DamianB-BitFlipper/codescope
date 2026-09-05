# 05 — AI-Assisted Visualization Design

Scope: how codescope uses an LLM to *choose and parameterize* visualizations of the current
change. The app owns facts, validation, and rendering; model claims never become authoritative
without validation. Interactive startup requires a configured AI provider. Deterministic impact
remains available outside the generated result, but a failed AI view reports failure explicitly
instead of impersonating a summary with fallback data.

Sources verified locally: HumanLayer Show Me SKILL.md (fetched 2026-08), crates.io versions,
`prime inference models` output, Prime Inference base URL from the prime-intellect skill docs.

## 1. Show Me skill — philosophy → rules for codescope

Read: `plugins/show-me/skills/show-me/SKILL.md` in `humanlayer/skills`. Its core ideas, and
the rule each becomes for codescope:

1. **"Pick the smallest view that makes the key point clear."**
   → Rule S1: a plan answers exactly one question. Its required `intent` is one concise sentence.
2. **Match the form to the question type** (its decision table: algorithm→pseudocode,
   runtime flow→call tree, UI structure→component tree, file responsibilities→shallow tree,
   interaction→sequence/flow, "what changes"→diff, mostly-new→whole block).
   → Rule S2: the AI picks from a fixed form enum (§2); no free-form drawing.
3. **"Match the diff shape to the topic"** — it diffs *structures* (component tree, file tree,
   call tree, state machine), not just text.
   → Rule S3: before/after and diff forms operate on symbol/tree structures; text diffs are
   the last resort, not the default.
4. **"Keep only the calls, files, props, states, and boundaries needed to answer the current
   question."** → Rule S4: the AI-facing plan allows at most 5 nodes per form; core retains a
   12-node defensive backstop. Depth is at most 3 and a plan has at most two forms. The TUI
   offers "expand" interactions instead of bigger plans.
5. **"Skip the preamble… you may use one of these, unlikely all of them."**
   → Rule S5: one screen, one visual. No dashboards. Prose summary ≤3 lines, rendered by the
   TUI next to the visual (Show Me: "place each visual next to the short text it supports").
6. **It uses the real labels/paths of the project.** → Rule S6: every label comes from a
   resolved fact-store entity, never from AI text (enforced by §3).

## 2. VisualizationPlan schema (AI output, TUI-rendered)

JSON, produced via a tool call (§5). Serde-friendly; versioned.

```json
{
  "plan_version": 6,
  "epoch": 7,
  "intent": "The handler checks its cache before persistent lookup.",
  "forms": [{
    "kind": "sequence",
    "nodes": [{
      "id": "n1",
      "entity": {
        "file": "src/session/store.rs",
        "symbol": "SessionStore.load"
      },
      "label": "check cache",
      "detail": "Checks cache before persistent lookup.",
      "code_refs": [{
        "file": "src/session/store.rs",
        "hunk": 0,
        "side": "new",
        "start_line": 122,
        "end_line": 126
      }],
      "change": "modified"
    }, {
      "id": "n2",
      "label": "read storage",
      "detail": "Reads storage after a cache miss.",
      "code_refs": [{
        "file": "src/session/store.rs",
        "hunk": 0,
        "side": "new",
        "start_line": 127,
        "end_line": 130
      }],
      "change": "modified"
    }],
    "edges": [{
      "from": "n1", "to": "n2", "kind": "flows_to", "label": "on cache miss"
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
`impact_summary` and `focused_diff` values do not pass the AI validation boundary. The required
plan fields are `plan_version`, `epoch`, and `intent`; AI submissions also require nonempty forms
and evidence, and every node requires `id`, `label`, `detail`, and one or two `code_refs`.
Language-neutral entities remain `(file, symbol, range)`. A node's `code_refs` are separate hover
anchors into the focused unified diff: file + zero-based hunk + explicit old/new side + inclusive
one-based lines. Every node must include at least one actual added/new-side or removed/old-side row;
unchanged context alone cannot ground a node.

Plan schema v6 adds the `flows_to` edge kind. It is a Sequence-only presentational adjacency
established by node order, not an impact-graph fact, so it bypasses impact-graph lookup. A
semantic Sequence edge instead requires graph proof. In a `relationship_flow`, fact-backed
`calls`, `imports`, `implements`, and `contains` edges are graph-validated; `reads` and `writes`
have no v0 graph counterpart and may remain presentational/unverified. This binary
deterministically rejects every `plan_version` other than 6 with a schema-version error;
persisted-plan migration is not required.

Pitfalls: (a) do not let the AI emit Mermaid/text art — TUI can't validate it; (b) `range`
must be optional for tree roots (file-level nodes: symbol omitted); (c) enforce caps at
validation, not in the prompt only.

## 3. Fact-validation boundary

Validation is local, deterministic, and has no AI in the loop. Pipeline:

1. **Epoch gate.** The server owns a repository-state epoch and bumps it for an accepted
   change-set. A plan carries the epoch from its prompt. If it differs from the current epoch,
   mark the plan *stale*, return the generated pane to `AI in progress`, and re-request it. A
   stale plan is never rendered as fresh.
2. **Entity resolution.** Every `entity` must resolve to exactly one fact-store entry
   (file exists, symbol exists in that file's outline, range within the symbol's extent or
   equal to it). Unresolvable = hallucination.
3. **Edge validation.** In a `sequence`, `flows_to` is presentational adjacency established
   by consecutive node order; it is Sequence-only and bypasses impact-graph lookup. A semantic
   Sequence edge requires graph proof: fact-backed `calls`, `imports`, `implements`, and
   `contains` must exist in the impact graph, so the AI may *select* them but never assert new
   ones. `reads` and `writes` have no v0 graph counterpart and cannot be presented as proven
   Sequence semantics. In a `relationship_flow`, those four fact-backed kinds are likewise
   graph-validated, while `reads` and `writes` may remain presentational/unverified. (If we later
   want hypothesis edges, add `"inferred": true`, render dashed; still require resolvable endpoints.)
4. **Hunk and code-link validation.** Plan evidence references `(file, hunk_index)`. Every
   AI node also carries one or two exact code ranges. The validator confirms the file/hunk and
   each one-based line on the declared old/new side against parsed diff rows before the range can
   drive hover highlighting and the temporary diff jump. Leaving the box restores the user's prior
   diff position; expanding a box does not pin its source highlight. Reversed, oversized, missing,
   or wrong-side ranges trigger a bounded repair; the model never emits screen coordinates.

Hallucination policy (per form):
- **Tree forms** (`changed_symbol_tree`, `call_tree`, `type_impl_tree`, `before_after`):
  drop the invalid node, re-parent its children to the node's parent, record a warning.
  If >20% of nodes invalid or the root invalid → reject the form.
- **Flow/sequence forms**: invalid endpoint breaks ordering semantics → reject the form.
- Any terminal rejection → one clickable `AI failed` banner with retained diagnostic detail.
  Log all drops/rejects; a "plan validation" debug pane helps tune prompts.

## 4. Current handoff policy + tool surface

The five-tier `ChangeDigest` is retained as historical analysis rationale and for deterministic
analysis/digest consumers: changed symbols, diagnostics, hunk summaries, one-hop relationships,
and a shallow repo sketch. It is **not** proactively sent as the fresh AI handoff.

The current AI flow is bounded differently:

1. The initial request contains the review assignment only (selection, virtual cwd, comparison
   scope, changed-file count, and assignment), not proactive source, hunks, diagnostics, symbols,
   or relationship lists.
2. The model obtains selection-scoped facts with tools. The controller first requires
   `list_directory` for a directory or `git_status_file` for a file/symbol so hunk choice follows
   an inventory instead of defaulting blindly to hunk zero. `git_diff_file` is authoritative for
   the comparison and code references.
3. Once a nonempty exact diff result is retained, every later request is a fresh compact handoff:
   the original assignment, retained exact diff results first (at most four), bounded successful
   supplementary reads tagged with their originating tool (at most eight), the current draft, and
   controller feedback/state. It never replays an old assistant/tool transcript. The retained
   research pool is at most 64 KiB; the complete handoff is at most 128 KiB. Exact diffs take
   priority over supplementary reads even when tools returned them later.

Current tool surface (OpenAI tool-calling format; paths are repo-relative and sandboxed to the
selection boundary; results are capped; the up-to-eight research/editor tools share at most 128
operations):

| tool | purpose |
|---|---|
| `list_directory` | bounded changed-file inventory for a directory selection |
| `read_file` | bounded surrounding source when the exact diff is insufficient |
| `search_changed_files` | bounded text search within changed files |
| `git_status_file` | compact status and hunk inventory |
| `git_diff_file` | authoritative annotated changed lines and zero-based hunk ids |
| `inspect_language_server` | capability-discovered symbols, relations, diagnostics, or hover facts |
| `edit_visualization` | one canonical incremental diagram mutation |
| `inspect_visualization` | current renderer-owned draft state |

Tool results are selection-bounded fact data. Final cited entities, hunks, lines, and eligible
semantic edges still pass deterministic validation; a tool result alone does not prove every
claim.

## 5. Provider-neutral client

**Wire protocols:** OpenAI Responses with function calling for the official OpenAI endpoint;
OpenAI-compatible Chat Completions for Prime Inference and custom endpoints; native Anthropic
Messages with `tool_use`/`tool_result` blocks for Anthropic. Verified targets include OpenAI
(`https://api.openai.com/v1`), Prime
Inference (`https://api.pinference.ai/api/v1`), and Anthropic
(`https://api.anthropic.com/v1`). Compatible local Chat Completions endpoints such as Ollama,
vLLM, and LM Studio can be selected with an explicit base URL. Native Anthropic requests carry
`x-api-key` plus `anthropic-version: 2023-06-01`; explicit supported reasoning levels use
`output_config.effort`. Tool continuations preserve Anthropic's complete signed assistant blocks,
put immediate `tool_result` blocks first in the following user turn, and set `is_error` on failures.

Plan construction uses shared incremental diagram tools. The selection-specific initial inventory
is controller-required; later research and normal full-schema construction turns use `Auto` tool
choice.
After exact diff evidence is retained, the initial
intent/form bootstrap and a focused recovery from a provider-truncated response use one
controller-selected canonical editor branch; the following normal turn returns to full-schema
`Auto`. OpenAI transports encode required turns in the request; Anthropic keeps provider tool
choice on `auto` for thinking-model compatibility and applies the requirement during controller
validation. Atomic edits mutate a bounded draft, inspection returns its current state, and a
tool-less full `Auto` turn requests deterministic validation/publication. A structurally complete
draft also validates after a small bounded refinement window or before an unsolicited destructive
rebuild, preventing endless edit cycles. There is no whole-plan
tool fallback or separate model completion tool.

**Crate choice:** `reqwest` 0.13.4 with the workspace's `json`, `stream`, and `rustls`
features, plus `serde`/`serde_json`. The direct client keeps exact control over OpenAI Responses,
compatible Chat Completions, and native Anthropic Messages payloads, authentication, tool choice,
usage accounting, and sanitized errors. HTTP streaming remains disabled; draft progress uses
ordinary bounded tool turns rather than SSE.

HTTP streaming remains off. Accepted draft edits remain available through the shared draft API;
between ordinary tool turns the TUI shows each research/edit call progressing through
running/succeeded/failed. Diagram publication remains atomic after validation.

Config resolution remains env-first, and every resolved configuration names a usable provider:
- Supported AI environment settings are `CODESCOPE_AI_BASE_URL` and `CODESCOPE_AI_TIMEOUT_MS`.
  An explicit base URL supports keyless local endpoints. Key resolution uses a config-file
  `api_key_env` when named, then
  `PRIME_API_KEY`, `OPENAI_API_KEY`, or `ANTHROPIC_API_KEY`; an arbitrary named key requires an
  explicit base URL. `PRIME_TEAM_ID` is optional for Prime Inference. `--model` / `-m` selects the
  model. Defaults follow the resolved key's Prime, OpenAI, or Anthropic provider. Keys are wrapped
  in `secrecy::SecretString` and never logged. HTTP streaming is off and has no environment toggle.

Privacy: the initial request sends the bounded assignment. Later fresh handoffs may also send
retained exact diffs and tagged tool results, the current diagram draft, bounded controller
feedback/state, and an explicitly untrusted prior validated design seed when available. File
bodies leave the machine only when the AI calls `git_diff_file`, `read_file`, or a supported
language-server inspection. Absolute paths, credentials, and environment values are scrubbed.

## Recommended decisions

1. AI selects from the structural form enum and incrementally describes boxes and relationships;
   the TUI owns all placement/rendering and reports generation failure explicitly.
2. Adopt Show Me's rules as plan constraints: one focus question, at most 5 AI-facing nodes per
   form (12 only as a core defensive backstop), depth ≤3, and structural diffs over text diffs.
3. Validate everything applicable: epoch gate, entity resolution, hunks by reference, and graph
   proof for fact-backed `calls`/`imports`/`implements`/`contains`. `flows_to` is Sequence-only
   adjacency from document order; `reads`/`writes` remain explicitly presentational/unverified in
   v0. Drop invalid nodes in tree forms (>20% or bad root → reject), reject broken flow/sequence
   forms outright, and report failure explicitly.
4. Keep context bounded: retained compact research is at most 64 KiB and the complete fresh
   handoff at most 128 KiB. The current research/editor tools share at most 128
   operations; retained exact Git diff results remain authoritative and take priority over tagged
   supplementary reads.
5. Client: reqwest + serde, OpenAI Responses / compatible Chat Completions and the shared
   incremental diagram tools; `CODESCOPE_AI_*` env config with `PRIME_API_KEY` auto-detection;
   interactive startup fails clearly when no provider credential or explicit local endpoint exists.
