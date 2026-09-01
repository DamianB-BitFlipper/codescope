# codescope

Understand what your current code changes do to the broader system — in your terminal.

codescope opens a repository and answers, at a glance:

- **What changed** on this branch (staged, unstaged, or branch-vs-base)
- **Which functions, methods, and types** contain those changes
- **What calls them and what they call** (from a real language server, not guesswork)
- **How the AI thinks the change is best explained** — clearly marked as interpretation
- **What's verified and what's approximate**

Go is the first supported language (via `gopls`). The design is language-neutral; see
[docs/architecture.md](docs/architecture.md).

## Status

Prototype. The core loop works end-to-end against a real `gopls`: git change detection →
change→symbol mapping → callers/callees/impact → optional AI-selected visualization → TUI.
The full workspace test suite passes; `clippy -D warnings` is clean.

## Build & run

Requires: Rust 1.85+, `gopls` on PATH (for semantic features), a git repository.

```sh
cargo build --release
./target/release/codescope [PATH]      # PATH defaults to .
```

Useful flags:

```sh
codescope --no-ai          # fully deterministic; no AI even if a key is set
codescope --model z-ai/glm-5.3  # model override for this run (-m is equivalent)
codescope --watch          # automatically refresh after repository changes (off by default)
codescope --log-file /tmp/cs.log   # tracing log (never contains secrets)
```

## Global configuration

Codescope keeps repository-independent preferences in
`$XDG_CONFIG_HOME/codescope/config.toml`, falling back to
`$HOME/.config/codescope/config.toml` (or the platform config directory on Windows).
Set `CODESCOPE_CONFIG` to use an explicit file. There is currently no repository-local
configuration.

The v1 TOML file can contain normal `[ai]` defaults plus the model last selected for each
provider and stable UI preferences:

```toml
version = 1

[ai]
# enabled = true
# model = "manual/fallback"
# api_key_env = "OPENAI_API_KEY" # names an env var; never put a key here

[ai.last_model]
prime = "openai/gpt-5-mini"
openai = "gpt-5-mini"
anthropic = "claude-haiku-4-5-latest"
# custom = "local-model"

[ui]
diff_wrap = false

[ui.dividers]
files_diff = 42
work_review = 16
relationships_generated = 52
selected_callers = 4
callers_downstream = 5
```

Model precedence is `--model` / `-m`, then the selected model remembered for the active
provider, then `[ai].model`, then the provider default. The CLI override applies only to
the current run. Codescope writes model
choices and stable UI preferences atomically; API keys and repository state are never
persisted. `api_key_env` recognizes the three built-in provider key names; an arbitrary
credential variable requires an explicit `base_url` so its value is never sent to a
guessed endpoint.

Open a Go repository with uncommitted or branch changes. The left pane lists changed files
and the symbols inside them; the center shows a focused diff. The combined Impact pane
stacks the selected change, callers, and downstream relationships on the left and keeps
the generated selection breakdown visible on the right. Every structural boundary is
draggable: files/diff, work/review, relationships/generated, selected/callers, and
callers/downstream. Divider positions are remembered globally.
The mouse wheel scrolls whichever section is under the pointer—Files, Diff, Callers,
Downstream, or the generated breakdown—without changing keyboard focus or the selected change.

## Comparison base

The branch scope compares your branch against a **base**. By default codescope uses the
configured `@{upstream}` when it produces a non-empty comparison. A same-tip upstream is
discarded. Codescope then considers both local and remote-tracking refs and chooses the
**nearest strict ancestor branch** by Git topology, rather than by timestamps or the
repository's default branch. For stacked branches `main ← feature-a ← feature-b`, the
default base of `feature-b` is `feature-a`, even when `feature-a` exists only as
`origin/feature-a`. Only when neither exists does it fall back to `origin/HEAD`,
`origin/main`/`origin/master`, or a fork-point guess. If none is meaningful, branch scope
reports that no base exists; staged, unstaged, and working scopes remain available.

Press `b` to open the base picker: it lists the upstream, ancestor branches, and the usual
default-branch candidates, marks the current base, and re-runs the whole analysis against
whichever ref you select (`j`/`k` move, `Enter` selects, `Esc` closes). The top bar always
labels the comparison as `base-ref ← checked-out-branch`, followed by the changed-file
count. Branch diffs always run from that resolved merge-base toward the checked-out `HEAD`
(`base → HEAD`).

## Keyboard controls

| key(s) | action |
|---|---|
| `q` / `Ctrl-C` | quit |
| `?` | help modal |
| `Tab` (files pane) | expand / collapse the selected file; symbols load automatically |
| `1` `2` `3` | focus files / diff / impact pane |
| `j`/`k` · `↑`/`↓` | move selection / scroll |
| `Ctrl-d`/`Ctrl-u`, `PgDn`/`PgUp` | half / full page in diff |
| `s` / `u` / `B` / `w` | scope: staged / unstaged / branch-vs-base / working (all uncommitted) |
| `S` | cycle scope |
| `Enter` | jump to symbol / re-center semantic view |
| `Space`, `h`/`l` | expand / collapse |
| `n` / `N` | next / previous diff hunk |
| `g` / `G` | top / bottom |
| `R` | refresh repository state |
| `b` | pick the comparison base for the branch scope |
| mouse hover (AI node) | highlight that node's exact linked old/new diff lines |
| click / `Space` (AI node) | expand or collapse deeper details and code locators |
| mouse wheel | scroll the section under the pointer without focusing/selecting it |
| mouse drag | resize any pane divider |

## Refresh mode

Codescope loads the repository once at startup and otherwise refreshes only when you press
`R`. Pass `--watch` to opt into automatic refreshes after working-tree or Git-state changes:

```bash
codescope --watch /path/to/repo
```

Scope and comparison-base selections remain explicit refreshes in either mode. Watch mode is
off by default.

## AI

AI is **off unless configured**. Set one of `PRIME_API_KEY`, `OPENAI_API_KEY`, or
`ANTHROPIC_API_KEY` (the first one found wins; the provider is inferred from the key), and
optionally use the global `[ai]` table, `CODESCOPE_AI_BASE_URL`, or
`--model <model_name>`. Providers that reject forced tool calls can use
`CODESCOPE_AI_TOOL_CHOICE=auto` (the default is `required`). Environment variables override
the global file. The app runs identically without it.
Press `m` in the TUI to switch models at runtime (fetches the provider's model list). AI output is a reviewer-first *visualization plan* — a title, intent, review focus, evidence, and at most two structural forms made of typed nodes and relationships — that the app validates against
known repository facts before rendering. Each node also carries one or two exact, side-aware
`code_refs` copied from annotated rows in the selected diff plus optional `expanded_detail`. Hovering a rendered
node highlights those old/new rows in the main diff; clicking it (or pressing `Space` while it is
hovered) pins its code highlight and opens its deeper explanation and source locators. Every cited file, hunk, source line,
symbol, or typed graph edge must resolve against the fact store; conceptual entityless nodes and
hunk-derived links are allowed as interpretation, rendered dashed and clearly marked inferred,
never as verified graph facts.
Changed-file symbols load asynchronously in the background. For the selected file/function,
AI generation starts automatically after its symbol inventory is complete and regenerates
after the refreshed change-set differs. With default manual refresh mode, press `R` after an
underlying file change; with `--watch`, that refresh happens automatically. Validated results
are cached per file/function. A regeneration receives the prior design as a continuity seed,
preserving useful structure for incremental edits while rebuilding when the change is
substantial; current repository facts are always revalidated and win over cached content.
Generation uses one provider lane and a priority queue: the focused row runs first, then
symbols in focused/expanded files, then file summaries for the remaining change-set. Moving
focus reprioritizes unsent work; an already-running plan finishes into its own row cache. Local
rate capacity is awaited asynchronously, and repeated failures pause background warming without
blocking a newly focused row. `debug-ai` uses the same pipeline in focused-only mode.
See [docs/ai-dataflow.md](docs/ai-dataflow.md).

### Headless backend debugging

The backend can generate the same selection-scoped, validated AI plan without starting a
terminal frontend. It drives the normal dispatcher actions and consumes the same snapshot
contract as the TUI:

```bash
# Explain the first changed file and print the full plan/debug envelope as JSON.
codescope debug-ai /path/to/repo --scope branch

# Target one changed file or function.
codescope debug-ai . --scope working --file src/server.rs
codescope debug-ai . --scope working --file src/server.rs --symbol 'Server::run'

# Inspect just the sentence shown above the diagram.
codescope debug-ai . --scope working --intent-only
```

The JSON includes the epoch, scope, selected file/function, provider, model, the complete
`VisualizationPlan` (`focus`, `title`, `intent`, forms, nodes with exact `code_refs`, edges,
review focus, expanded details, and evidence),
and the full validation report (verdict, dropped items, notes).
`debug-ai` reads the same global AI configuration and environment overrides as the TUI.
Use `digest --text` when you only need the pre-AI repository digest.

For example, an OpenAI-compatible provider that only accepts automatic tool selection:

```bash
CODESCOPE_AI_TOOL_CHOICE=auto \
  codescope debug-ai . --scope branch --model 'z-ai/glm-5.3' --timeout-secs 180
```

## Documentation

- [docs/architecture.md](docs/architecture.md) — design decisions and crate layout
- [docs/adding-a-language-server.md](docs/adding-a-language-server.md) — extend past Go
- [docs/ai-dataflow.md](docs/ai-dataflow.md) — the AI integration and validation boundary
- [docs/limitations.md](docs/limitations.md) — known limitations and next improvements
- [docs/research/](docs/research/) — the research notes each decision cites
