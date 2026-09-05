# codescope

Understand what your current code changes do to the system—without leaving the terminal.

Codescope turns a Git comparison into a navigable review workspace: changed files and symbols on
the left, an exact diff in the center, and code relationships plus a validated AI diagram on the
right.

## Highlights

- **Review the comparison you mean.** Switch among committed branch changes, branch plus working
  changes, staged, unstaged, and all uncommitted changes, with explicit base selection for
  branch-based reviews.
- **Navigate by code, not just files.** Changed hunks are mapped to functions, methods, and types.
- **Follow real relationships.** Callers, callees, implementations, references, diagnostics, and
  syntax information come from the repository's language server.
- **See the change as a diagram.** AI builds compact boxes and relationships backed by exact diff
  ranges; Codescope validates every cited file, hunk, and line before publishing it.
- **Keep your place.** Diff scroll, diagram positions, expanded boxes, and review state survive
  navigation for the current session.
- **Work with coding agents.** The bundled skill lets an external agent inspect the live review and
  edit the same validated diagram through the `codescope agent` CLI.
- **Stay in control.** Codescope never edits the repository. Telemetry is append-only, local-only,
  and secret-scrubbed.

Codescope currently has production language-server adapters for Go (`gopls`) and Rust
(`rust-analyzer`). Git-only review remains available when semantic support is unavailable.

## Installation

Codescope is currently installed from source. It requires Rust 1.85 or newer:

```bash
git clone https://github.com/DamianB-BitFlipper/codescope.git
cd codescope
cargo install --path crates/codescope --locked
```

For semantic navigation, install the language server for the repository you want to review:

```bash
go install golang.org/x/tools/gopls@latest   # Go
rustup component add rust-analyzer           # Rust
```

The interactive application also requires an AI provider. Set one of `PRIME_API_KEY`,
`OPENAI_API_KEY`, or `ANTHROPIC_API_KEY`, or configure an explicit OpenAI-compatible base URL.

## Quick start

Open any directory inside a Git worktree:

```bash
export OPENAI_API_KEY="..."
codescope .
```

Codescope starts with the branch comparison. Then:

1. Move through changed directories, files, and symbols with `j`/`k` or the mouse.
2. Press `Enter` to center the selected code in the diff.
3. Press `a` to generate a diagram for the current selection.
4. Hover a diagram box to highlight its cited lines; click it or press `Space` to expand it.
5. Press `v`, or click a review marker, to mark a directory, file, or symbol reviewed.

Press `?` at any time for the complete in-app key reference.

### Choose the comparison

| Key | Comparison |
|---|---|
| `s` | next: branch → branch+working → staged → unstaged → working |
| `S` | previous comparison |
| `b` | choose the branch base |
| `g` | refresh repository state and clear session AI impacts |

`branch` compares the resolved merge base to `HEAD`; `branch+working` compares that same base to
the current worktree, combining committed branch changes with staged, unstaged, and untracked work.
For both branch-based comparisons, Codescope prefers a meaningful upstream and otherwise finds the
nearest strict ancestor branch by Git topology. The base picker can override that choice. Pass
`--watch` to refresh automatically when the worktree or Git state changes; otherwise refreshes are
explicit.

Completed AI impacts are retained while switching among comparison scopes during the session.
Codescope restores them only when the returning parsed comparison is identical; press `g` to clear
the session's generated impacts and rebuild from repository state.

## The review workspace

### Changed files

The changed-files pane is a directory → file → LSP object tree. Directories are expanded by default;
`Tab` collapses or expands the selected directory or file without starting AI work.
Moving the selection onto an LSP object centers its first mapped changed row in the diff. Each
object then retains that viewport independently for the rest of the session.

Review marks are hierarchical and content-aware:

- Marking a directory or file covers every current descendant.
- Unmarking it removes only that parent override, preserving files or LSP objects marked independently.
- A changed file becomes unreviewed after refresh instead of inheriting a stale decision.
- `●`, `↳`, `✓`, and `◐` distinguish explicit, inherited, complete, and partial review state.

### Diff

The center pane renders exact old/new Git rows, with syntax colors when the language server
supports them. Codescope requests colors on first view and also fills a bounded syntax cache in the
background; whichever path reaches a file first supplies the same cached result. Each file
remembers its scroll position while you navigate elsewhere. Hovering a diagram box temporarily
centers and highlights its cited lines; clicking the box keeps the jumped position. Trackpad
gestures lock to their dominant axis, with a small horizontal dead-zone so vertical reading does
not drift an unwrapped diff sideways.

### Impact and diagrams

When semantic relationships exist, the Impact pane shows callers and downstream symbols beside
the generated view. With no relationships, the diagram gets the full width.

AI generation is manual by default: press `a` for the selected directory, file, or symbol. Press
`A` to enable selection-following generation for the session, and `m` to switch model or reasoning
effort. During generation, Codescope shows the complete research and diagram-tool activity rather
than rendering an unfinished draft.

Published diagrams are grounded in the captured comparison. Nodes carry exact, side-aware diff
references, and fact claims are checked against Git and language-server results. Interpretive
control-flow links remain visibly inferred. Boxes can be moved and independently expanded; their
positions and expansion states are remembered per selection for the session.

## Skills

Codescope distributes an official skill that teaches coding agents how to research the selection
in a running Codescope review and build or revise its validated diagram. The skill is bundled with
the CLI, so its instructions stay aligned with the installed Codescope version.

Install it in the current project:

```bash
codescope skills install
```

Or manage another supported location:

```bash
codescope skills install --global   # ~/.agents/skills/codescope
codescope skills install --claude   # .claude/skills/codescope
codescope skills update             # update the project installation
codescope skills update --global    # update the global installation
codescope skills show               # print the bundled skill
```

Installation and updates ask for confirmation. Pass `--yes` for non-interactive use.

Once installed, ask your coding agent to use Codescope directly:

```text
Use $codescope to explain the selected change and add the failure path to the diagram.
```

The skill uses `codescope agent` to read the live selection, obtain authoritative diff
coordinates, and apply typed diagram edits. It performs broader research with the coding agent's
normal repository tools. On Unix, the live connection is an owner-only socket derived from the
canonical project directory. Sandboxed agents may require one host approval to access it; the
skill recognizes `PermissionDenied`/`EPERM` and retries once through the host's normal escalation
mechanism instead of incorrectly claiming the TUI is absent.

Only one Codescope TUI can own a canonical project directory at a time. Separate Git worktrees
have separate socket identities.

## Commands

Running `codescope [PATH]` opens the TUI. Codescope also provides JSON-oriented commands for
scripts and debugging:

```bash
codescope scan .
codescope changeset --scope branch-working .
codescope changeset --scope working .
codescope analyze --scope branch .
codescope digest --scope staged --text .
codescope bases .
codescope debug-ai --scope branch --file src/server.rs .
```

Use `--compact` with JSON commands for a single-line response. Run `codescope help` or
`codescope <command> --help` for the complete command reference.

### Live agent control

A running TUI exposes a local control API on Unix:

```bash
codescope agent . context
codescope agent . focus --file src/server.rs --symbol run
VIEW_ID='<result.view_id from context>'
codescope agent . diff --view-id "$VIEW_ID" --file src/server.rs --hunk 0
codescope agent . diagram inspect --view-id "$VIEW_ID"
codescope agent . diagram schema
codescope agent . diagram finish --view-id "$VIEW_ID"
codescope agent . refresh
```

`context` returns the live selection and an opaque `view_id` tied to that exact selection, captured
diff, and repository epoch. Pass it to every `diff`, diagram `inspect`/`edit`, and diagram `finish`
command.
Those commands continue targeting the captured view even if the user navigates elsewhere; they do
not move or replace the active viewport. A refresh or comparison change invalidates old IDs with a
clear stale-view error. `diff` returns the exact zero-based hunk identity and one-based source
coordinates required by diagram evidence. Diagram edits use the same typed API and validator as
Codescope's internal AI. The protocol cannot execute a shell command or write to Git.

### Essential controls

| Key | Action |
|---|---|
| `1` / `2` / `3` | focus files / diff / impact |
| `j` / `k`, `↑` / `↓` | move or scroll |
| `Ctrl-d` / `Ctrl-u`, `PgDn` / `PgUp` | scroll the diff |
| `n` / `N` | next / previous hunk |
| `Enter` | jump to the selected symbol |
| `Space`, `←` / `→` | toggle or explicitly collapse / expand the targeted item |
| `a` / `A` | generate AI / toggle automatic generation |
| `m` | choose model and reasoning effort |
| `s` / `S` | next / previous comparison scope |
| `v` | toggle the selected change's reviewed state from any pane |
| `?` | show help |
| `Q`, `Ctrl-C` | quit |

The mouse can select tree rows, scroll individual panes, resize every visible divider, drag diagram
boxes, open relationships, toggle review markers, and select diff text for copying.

## Configuration

Codescope stores global configuration at `$XDG_CONFIG_HOME/codescope/config.toml`, falling back to
`$HOME/.config/codescope/config.toml` or the platform configuration directory on Windows. Set
`CODESCOPE_CONFIG` to use another file. There is no repository-local configuration.

A minimal configuration looks like:

```toml
version = 1

[ai]
# model = "openai/gpt-5.6-luna"
# reasoning_effort = "default"
# api_key_env = "OPENAI_API_KEY"

[ui]
diff_wrap = false
```

Command-line model and reasoning options override remembered provider choices, which override the
global defaults:

```bash
codescope --model openai/gpt-5.6-luna --reasoning-effort high .
```

`CODESCOPE_AI_BASE_URL` configures a custom OpenAI-compatible endpoint. An arbitrary credential
environment variable requires an explicit base URL so Codescope never sends it to a guessed
provider. The official OpenAI base uses the Responses API so reasoning and function tools work
together; Prime and custom compatible providers continue to use Chat Completions. The official
Anthropic base uses the native Messages API with `x-api-key` and the required API-version header.
Anthropic `low` through `max` reasoning settings map to `output_config.effort` when the selected
model supports them; `none` and `minimal` are rejected locally.

## Privacy and telemetry

Codescope writes one append-only JSONL telemetry stream per process/session in a `telemetry/`
directory beside the global config. It is always on, local only, owner-readable on Unix, and has no
upload path.

Telemetry records UI interactions, navigation, controller activity, and the provider envelopes
needed to reconstruct LLM trajectories. Every record has an explicit `application`, `user`,
`internal_agent`, or `external_agent` origin. External commands share a structured command ID,
operation, view ID, phase, and status across their CLI and TUI streams. Each comparison is stored
once as a complete, content-addressed `diff.snapshot`; later events reference its
`diff_snapshot_id`. Excluded files,
authorization headers, API keys, recognizable secrets, and absolute repository paths pass through
the privacy and scrubbing pipeline before storage. See the [telemetry contract](docs/telemetry.md)
for the complete event and correlation model.

Verbose debugging is opt-in:

```bash
codescope --debug .
codescope --debug --log-file /tmp/codescope-debug.log .
```

Debug logs can contain scrubbed repository source and model output. Review them before sharing.

## Language support and limitations

Go and Rust are the supported semantic adapters today. Other repositories still get Git diff and
AI review, but not language-server relationships or syntax semantics.

Static analysis is inherently incomplete around dynamic dispatch, reflection, generated code,
build tags, dependency injection, and language-server indexing limits. Codescope exposes partial
or approximate evidence instead of presenting it as complete. See [known limitations](docs/limitations.md).

Codescope is currently a prototype. The Git → symbol → relationship → validated-diagram loop works
end to end, but interfaces and storage formats may still change.

## Development

Build and run from the workspace:

```bash
cargo build --release
./target/release/codescope .
```

Before submitting changes:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

## Documentation

- [Architecture](docs/architecture.md) — crate boundaries and system design
- [AI data flow](docs/ai-dataflow.md) — research, diagram construction, and validation
- [Telemetry](docs/telemetry.md) — local event schema, snapshots, and privacy
- [Adding a language server](docs/adding-a-language-server.md) — extending semantic support
- [Known limitations](docs/limitations.md) — accuracy boundaries and next improvements
- [Research notes](docs/research/) — evidence behind the design decisions

## License

Codescope is licensed under the MIT License.
