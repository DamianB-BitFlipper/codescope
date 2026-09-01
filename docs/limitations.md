# Known limitations

Honest list, in roughly decreasing order of impact.

## Semantic completeness (inherent)

Static analysis is **incomplete by nature**. codescope's impact graph can miss real
relationships because of:

- **dynamic dispatch** — interface satisfaction is found, but which implementation runs at a
  call site is a runtime property
- **reflection** (`reflect`, `interface{}` type assertions)
- **generated code** (stringer, mockgen, protobuf) not present or not indexed
- **build tags / conditional compilation** — gopls indexes the default build configuration
- **dependency injection and runtime registration** — wiring that only exists at runtime
- **language-server limits** — gopls returns partial or no results while indexing, under heavy
  build-tag matrices, or for some cross-module references

codescope surfaces this as `Evidence` completeness and `~`/`?` confidence markers rather than
claiming a complete project graph. There is no runtime data-flow analysis in v0.

## Prototype-scope limitations

- **Go only.** rust-analyzer/clangd/pyright/tsls are designed for but not implemented.
- **gopls document sync** uses close+reopen with full text (correct, simple; not incremental).
- **AI plan entities** for implementation/reference results carry range-derived placeholder
  names (e.g. `42:8`) until hover-based name enrichment lands; the *positions* are exact.
- **Hunk citations are index-based** — plan evidence references hunks by zero-based diff
  index (rendered one-based) rather than a stable `HunkId`; the legacy `focused_diff` form
  that used index addressing is no longer accepted at the AI plan boundary.
- **Submodules, symlinked roots, and `.gitignore`-driven AI exclusions** are handled
  conservatively; edge cases (linked worktrees with unusual layouts) are untested.
- **The TUI renders the first changed file's diff and the full impact graph**; per-selection
  diff/call-tree re-centering is wired but shallow.
- **No `$/cancelRequest`** to gopls — superseded in-flight requests finish and their results
  are dropped by epoch check (sufficient, slightly wasteful).

## Highest-value next improvements

1. **Hover/signature enrichment** for graph node labels (real names for implementation refs).
2. **Incremental LSP sync** (gopls supports it) to cut churn on rapid edits.
3. **Per-selection semantic views** — re-center the impact/call tree on the focused symbol.
4. **rust-analyzer adapter** to prove the multi-language boundary.
5. **A plan-validation debug pane** to tune prompts against the drop/reject report.
6. **Deeper privacy filters** — content sniffing for secrets in outgoing digests.
