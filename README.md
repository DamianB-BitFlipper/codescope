# codescope

Understand what your current code changes do to the broader system — in your terminal.

codescope opens a repository and answers, at a glance:

- **What changed** on this branch (staged, unstaged, or branch-vs-base)
- **Which functions, methods, and types** contain those changes
- **Syntax-highlighted old and new diff lines** when the file's language server supports them
- **What calls them and what they call** (from a real language server, not guesswork)
- **How the AI thinks the change is best explained** — clearly marked as interpretation
- **What's verified and what's approximate**

Go is the first supported language (via `gopls`). The design is language-neutral; see
[docs/architecture.md](docs/architecture.md).

## Status

Prototype. The core loop works end-to-end against a real `gopls`: git change detection →
change→symbol mapping → callers/callees/impact → AI-generated visualization → TUI.
The full workspace test suite passes; `clippy -D warnings` is clean.

## Build & run

Requires: Rust 1.85+, `gopls` on PATH (for semantic features), a git repository, and an AI
provider configured through `PRIME_API_KEY`, `OPENAI_API_KEY`, or `ANTHROPIC_API_KEY`.

```sh
cargo build --release
./target/release/codescope [PATH]      # PATH defaults to .
```

Useful flags:

```sh
codescope --model z-ai/glm-5.3  # model override for this run (-m is equivalent)
codescope --reasoning-effort high # reasoning override (-r; default uses automatic behavior)
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
# model = "openai/gpt-5-mini"
# reasoning_effort = "default"
# api_key_env = "OPENAI_API_KEY" # names an env var; never put a key here

[ai.last_model]
prime = "openai/gpt-5-mini"
openai = "gpt-5-mini"
anthropic = "claude-haiku-4-5-latest"
# custom = "local-model"

[ai.last_reasoning_effort]
prime = "minimal"
openai = "medium"
# custom = "default"

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
the current run. Reasoning effort follows the same precedence through
`--reasoning-effort` / `-r`; `default` uses automatic provider/model behavior (normally
omitting the parameter, with a `minimal` compatibility default for Prime-hosted GLM). Codescope writes model
and reasoning choices plus stable UI preferences atomically; API keys and repository state are never
persisted. `api_key_env` recognizes the three built-in provider key names; an arbitrary
credential variable requires an explicit `base_url` so its value is never sent to a
guessed endpoint.

Open a Go repository with uncommitted or branch changes. The left pane lists changed files
and the symbols inside them; the center shows a focused diff. Syntax colors arrive
asynchronously from the language server for the visible file and fall back silently to the plain
diff when unavailable. The combined Impact pane
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
| `Tab` (files pane) | expand / collapse the selected directory or file; symbols load automatically |
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
| click / `Space` (AI node) | expand or collapse that box in place with its full detail and source refs |
| click AI relationship | toggle its complete text in an overlay without moving the diagram |
| drag AI node | move the box freely in X/Y; connected arrows follow it |
| mouse wheel | scroll the section or relationship overlay under the pointer without changing focus |
| mouse drag | resize any pane divider |
| drag diff code | select code without line-number gutters; release copies it |
| click diff | clear the retained text selection |

The changed-files pane is a directory → file → symbol tree. Selecting a directory produces a
module-level summary over only the changed files below it; selecting a file or symbol narrows the
summary to that entry. Every selectable row carries an AI state marker: `◆` ready, `◇` not
generated, `◌` generating, or `!` failed. Directories are expanded by default and collapse
locally without starting or cancelling inference. Directory-only chains are combined into one
path row and split only at real branch points; directory totals show a bare file count followed
by added/deleted lines.

## Refresh mode

Codescope loads the repository once at startup and otherwise refreshes only when you press
`R`. Pass `--watch` to opt into automatic refreshes after working-tree or Git-state changes:

```bash
codescope --watch /path/to/repo
```

Scope and comparison-base selections remain explicit refreshes in either mode. Watch mode is
off by default.

## AI

AI is required by the interactive application. Set one of `PRIME_API_KEY`, `OPENAI_API_KEY`, or
`ANTHROPIC_API_KEY` (the first one found wins; the provider is inferred from the key), and
optionally use the global `[ai]` table, `CODESCOPE_AI_BASE_URL`, `--model <model_name>`, or
`--reasoning-effort <default|none|minimal|low|medium|high|xhigh|max>`. Environment variables
override the global file. Interactive startup exits with a configuration error when AI is absent
or explicitly disabled; there is no no-AI mode.
Press `m` in the TUI to switch models at runtime (fetches the provider's model list); use
left/right in that picker to stage reasoning effort, then Enter to apply both settings once.
AI incrementally builds a reviewer-first *visualization draft* — an intent, evidence, and at
most two structural forms made of typed boxes and relationships. Each accepted tool edit updates
the controller-visible draft; when the model naturally ends its tool sequence, Codescope validates
and publishes the accumulated result against known repository facts. The model can inspect, update, and delete existing boxes and relationships
instead of repeatedly returning the whole plan. Each node also carries one or two exact, side-aware
`code_refs` copied from annotated `git_diff_file` tool results plus optional
`expanded_detail`. The model describes semantics, not coordinates. The renderer seeds a responsive
two-dimensional canvas, keeps every box inside the pane width, and uses one vertical scroll axis
when the result is taller than the viewport. Dragging a box stores its session-local X/Y position;
every connected arrow is routed again from the box's current bounds. Clicking a box grows that same
box in place to show its complete detail and source refs. Clicking an arrow toggles its complete text
in a top-layer overlay. Long overlay text pages with the mouse wheel. Opening, paging, or closing the
overlay does not change any box, arrow, canvas extent, or base scroll position. While an
internal request
is running, the generated pane shows only `AI in progress` and its research/diagram tool calls
progressing from running to succeeded/failed. The complete call history remains vertically
scrollable, and failed calls include their scrubbed error reason. Draft boxes never render. A terminal failure
shows one clickable `AI failed` banner; it does not replace the unfinished draft with known
relationships. Hovering a rendered
node highlights those old/new rows in the main diff; clicking it (or pressing `Space` while it is
hovered) pins its code highlight and expands the box in place with deeper detail and source locators.
Every cited file, hunk, source line,
symbol, or typed graph edge must resolve against the fact store; conceptual entityless nodes and
hunk-derived links are allowed as interpretation, rendered dashed and clearly marked inferred,
never as verified graph facts.
Changed-file symbols load asynchronously in the background. For a selected file/function,
AI generation starts automatically after its symbol inventory is complete; a selected directory
can start after the same navigation debounce because its summary does not require every child
symbol inventory to finish. Both regenerate after the refreshed change-set differs. With default
manual refresh mode, press `R` after an underlying file change; with `--watch`, that refresh
happens automatically. Validated results are cached per directory/file/function. A regeneration receives the prior design as a continuity seed,
preserving useful structure for incremental edits while rebuilding when the change is
substantial; current repository facts are always revalidated and win over cached content.
Only the current directory, file, or function is sent for AI generation, after a 250 ms selection
debounce; there is no prompt prefetch or background generation. The first turn is a small research
brief rather than a source dump. A bounded agentic loop can list the selection, read sections of
changed files, search changed files, inspect captured per-file Git status/diffs, and explore the
active language server through `inspect_language_server`. That capability-discoverable tool can
anchor queries at the current symbol, an exact symbol name, or a source position and returns
symbols, references, callers/callees, implementations, type relationships, diagnostics, hover,
and semantic tokens when supported by the active adapter. Every response identifies its epoch,
worktree revision, completeness, notes, and truncation; facts actually returned by relationship
queries join the validator's evidence catalog for that generation. These tools
use a virtual cwd; file tools also accept exact repo-relative paths or an unambiguous repo-path suffix.
They reject absolute paths and `..`, cannot execute commands or leave the selection, and return
capped results. A plan is accepted only after at least one research call succeeds.
Moving focus cancels the unsent debounce,
while an already-started plan finishes into its original row cache. Up to 16 requests may remain
active; starting request 17 aborts the oldest active request. The local provider limiter primarily
bounds actual HTTP work to 8 concurrent requests; a 600-requests/minute token bucket with burst
100 remains only as a high safety ceiling, and capacity is awaited asynchronously. `debug-ai`
uses the same selection-only pipeline.
See [docs/ai-dataflow.md](docs/ai-dataflow.md).

### Live agent control

On Unix platforms, every running TUI exposes a repository-specific local socket. The `codescope agent`
client lets another coding agent inspect the exact backend snapshot, move the visible tree
selection, ask a selection-scoped question, give feedback, incrementally edit the exact same
diagram draft used by the internal AI, or refresh the repository:

```bash
# Read the live tree, selected diff, relationships, and validated AI plan/report.
codescope agent . context

# Move the selection shown by the TUI. Loaded symbols can be targeted by name.
codescope agent . focus --directory crates/codescope
codescope agent . focus --file crates/codescope/src/main.rs --symbol main

# Generate an answer in the visible Impact pane, then refine it.
codescope agent . ask 'How does this change alter request routing?'
codescope agent . feedback 'Make the queue boundary and failure path more prominent'

# Inspect and edit the live renderer-native draft with the shared DiagramCommand JSON API.
codescope agent . diagram show
codescope agent . diagram apply '{"op":"update_edge","form_id":"main","from":"n1","to":"n2","patch":{"label":"passes parsed request"}}'
codescope agent . diagram apply '{"op":"delete_edge","form_id":"main","from":"n1","to":"n3"}'
codescope agent . diagram finish

codescope agent . refresh
```

Focus, generation, and refresh are asynchronous; call `context` again to observe the next
snapshot and check `live.epoch`, `live.refreshing`, `ai.status`, and `ai.plan`. Commands enter
the same typed action/selection path as keyboard and mouse input, so a CLI focus updates the
actual visible cursor. Questions and feedback are bounded, explicitly labelled as presentation
guidance rather than source evidence. `diagram apply` accepts the same tagged command objects as
the model's `edit_visualization` tool (`set_intent`, form/node/edge create-update-delete, and
evidence add/delete); `diagram finish` runs the same fact and visualization validators used by
internal inference.

The socket lives in the platform temporary directory, is derived from the canonical repository
root, and is permissioned for its owner only. The protocol has no shell, file-write, Git-write,
or raw UI-injection operation. `codescope agent . socket` prints its path; `--compact` produces
single-line JSON for tool integrations.

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

For example, a longer-running OpenAI-compatible request:

```bash
codescope debug-ai . --scope branch --model 'z-ai/glm-5.3' \
    --reasoning-effort minimal --timeout-secs 180
```

## Documentation

- [docs/architecture.md](docs/architecture.md) — design decisions and crate layout
- [docs/adding-a-language-server.md](docs/adding-a-language-server.md) — extend past Go
- [docs/ai-dataflow.md](docs/ai-dataflow.md) — the AI integration and validation boundary
- [docs/limitations.md](docs/limitations.md) — known limitations and next improvements
- [docs/research/](docs/research/) — the research notes each decision cites
