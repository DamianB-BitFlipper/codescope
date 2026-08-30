# AI data flow and the validation boundary

codescope keeps **facts** and **interpretation** strictly separate. Git, the language server,
and source analysis produce facts. The AI only *selects and arranges* facts into a clear view.

## What the AI never does

- invent symbols, files, calls, references, types, or relationships
- get a shell, the filesystem, or the network
- write to the repository
- replace the deterministic visualization with unverified content

The app is fully functional with AI disabled, unavailable, slow, or rate-limited.

## The pipeline

1. **Detect** — watchers (working tree + git dir, debounced) notice a change; the dispatcher
   bumps the repo-state **epoch** (a `u64` generation).
2. **Refresh** — git re-reads the change-set; the analysis engine maps hunks to symbols and
   builds the 1-hop impact graph from the language server.
3. **Deterministic first** — the TUI immediately renders the deterministic impact view.
4. **Digest** — a compact description (changed symbols, diagnostics, hunk summaries, 1-hop
   relationships, repo sketch; ~4–8k tokens, hard-capped) is assembled. The whole repository is
   never sent; absolute paths are stripped.
5. **Request** — the AI returns a `VisualizationPlan` (a fixed 8-form enum: changed-symbol tree,
   call tree, type/impl tree, relationship flow, impact summary, focused diff, before/after,
   sequence) via a required `submit_visualization_plan` tool call. It may first call read-only
   tools (get_callers, get_hunk, …; ≤8 calls) whose results come from the fact store, so
   anything it can cite is guaranteed resolvable.
6. **Validate** — every entity in the plan must resolve against the fact store (file exists,
   symbol resolves, edge exists in the impact graph, hunk index valid). Hallucinated nodes are
   dropped (tree forms) or the form is rejected (flow/sequence). Caps are enforced
   (≤12 nodes, depth ≤3, ≤2 forms). Invalid results fall back to the deterministic view.
7. **Epoch gate** — the plan carries the epoch it was requested against. If the repository has
   changed since, the plan is **stale**: the last valid render stays on screen with a "stale"
   badge and a new request is issued. A stale plan can never replace a newer state's view.

## Failure modes

- **No API key** → AI status "off"; deterministic views only.
- **Timeout / rate limit** — 20 s request timeout, `Retry-After` honored, exponential backoff;
  a circuit breaker (3 transport failures in 60 s) cools down for 60 s.
- **Malformed / hallucinated plan** — validation drops or rejects; deterministic fallback shows;
  the status line notes it.
- **Rapid edits** — changes are coalesced; the AI is only re-asked when the change-set meaningfully
  differs, never on every keystroke.

## Provider neutrality

The client speaks OpenAI-compatible `chat/completions` with tool calling. Verified against Prime
Inference (`https://api.pinference.ai/api/v1`) and OpenAI; anything compatible (Ollama, vLLM, …)
works by setting the base URL. The default model is a small, schema-constrained one — plans are
structured, so they don't need a frontier model.

## Privacy

- API keys come from **environment variables only** (`CODESCOPE_AI_API_KEY` > `PRIME_API_KEY` >
  `OPENAI_API_KEY`); a literal key in a config file is a hard error.
- Keys are wrapped in `secrecy::SecretString`; never logged, never shown.
- Only the digest (repo-relative paths, symbol names, hunk summaries) leaves the machine; file
  bodies are sent only if the AI explicitly requests a hunk/symbol via a tool.
- The status bar always shows whether AI is on, loading, ready, stale, or failed.
