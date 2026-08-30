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
365 tests, `clippy -D warnings` clean.

## Build & run

Requires: Rust 1.85+, `gopls` on PATH (for semantic features), a git repository.

```sh
cargo build --release
./target/release/codescope [PATH]      # PATH defaults to .
```

Useful flags:

```sh
codescope --no-ai          # fully deterministic; no AI even if a key is set
codescope --log-file /tmp/cs.log   # tracing log (never contains secrets)
```

Open a Go repository with uncommitted or branch changes. The left pane lists changed files
and the symbols inside them; the center shows a focused diff; the right shows callers,
callees, implementations, or an impact view for the selection.

## Keyboard controls

| key(s) | action |
|---|---|
| `q` / `Ctrl-C` | quit |
| `?` | help modal |
| `Tab` / `Shift-Tab`, `1` `2` `3` | focus files / diff / semantic pane |
| `j`/`k` · `↑`/`↓` | move selection / scroll |
| `Ctrl-d`/`Ctrl-u`, `PgDn`/`PgUp` | half / full page in diff |
| `s` / `u` / `B` | scope: staged / unstaged / branch-vs-base |
| `S` | cycle scope |
| `Enter` | jump to symbol / re-center semantic view |
| `Space`, `h`/`l` | expand / collapse |
| `+` / `-` | semantic expansion depth |
| `n` / `N` | next / previous diff hunk |
| `g` / `G` | top / bottom |
| `R` | rescan git |
| `a` / `A` | AI toggle / force AI refresh |

## AI

AI is **off unless configured**. Set one of `PRIME_API_KEY`, `OPENAI_API_KEY`, or
`ANTHROPIC_API_KEY` (the first one found wins; the provider is inferred from the key), and
optionally `CODESCOPE_AI_BASE_URL` / `CODESCOPE_AI_MODEL`. The app runs identically without it.
Press `m` in the TUI to switch models at runtime (fetches the provider's model list). AI output is a *visualization plan* that the app validates against
known repository facts before rendering; the AI can never invent symbols, files, or calls.
See [docs/ai-dataflow.md](docs/ai-dataflow.md).

## Documentation

- [docs/architecture.md](docs/architecture.md) — design decisions and crate layout
- [docs/adding-a-language-server.md](docs/adding-a-language-server.md) — extend past Go
- [docs/ai-dataflow.md](docs/ai-dataflow.md) — the AI integration and validation boundary
- [docs/limitations.md](docs/limitations.md) — known limitations and next improvements
- [docs/research/](docs/research/) — the research notes each decision cites
