---
name: codescope
description: Research a live Codescope review with the host agent's normal code tools, then inspect and build its validated diagram through the local `codescope agent` CLI. Use for explaining or revising the change currently selected in a running Codescope TUI.
---

# Codescope Live Review

Own the investigation yourself. Codescope supplies the human's live review context, authoritative diff coordinates, shared diagram editor, and validator; it does not supply a second research agent through this workflow.

## Establish the live target

Run from any directory inside the repository:

```bash
codescope agent . context
```

Successful live commands return `{protocol_version, ok, result}`. Treat `result.repository.root`, `result.live.epoch`, `result.live.scope`, `result.live.selection`, `result.changed_tree`, `result.focused_diff.selected`, and the current `result.ai.draft`/`result.ai.plan` as authoritative live state. A selected diff excerpt is an attention anchor, not complete evidence.

If the user asks to change focus, use an exact directory, file, or loaded symbol from `context`, then read context again because focus is asynchronous:

```bash
codescope agent . focus --directory crates/codescope
codescope agent . focus --file crates/codescope/src/main.rs --symbol main
codescope agent . context
```

If connection fails, report that Codescope must be running in that repository. Do not start another TUI unless the user explicitly asks. Multiple TUI instances for one canonical repository are unsupported.

## Research with host tools

Use your normal filesystem, search, Git, and language-aware tools from `result.repository.root`. You may inspect unchanged code, callers, dependencies, and tests when they help explain the selected change. Keep the final diagram about `result.live.selection`; broader code is supporting context, not proof that behavior changed.

Do not modify the repository merely because this skill is active. Repository writes still require the user's surrounding request to authorize them.

## Obtain exact change evidence

Use CodeScope's captured diff after research to translate conclusions into validator-compatible citations. Omit `--file` to use the focused file, or pass any exact changed path from `changed_tree` without moving the TUI selection:

```bash
codescope agent . diff --file crates/codescope/src/main.rs
codescope agent . diff --file crates/codescope/src/main.rs --hunk 0
```

The overview returns zero-based hunk ids. A hunk response returns exact one-based `old_line` and `new_line` coordinates. Use `side: "old"` for deleted lines and `side: "new"` for added or post-change context. Use `--offset N` to page a hunk when `next_offset` is present.

Each diagram node needs one or two `code_refs`, and at least one referenced row must be added or deleted. Keep every reference within one file, hunk, and side. Evidence items use the same exact changed file and zero-based hunk.
For file or symbol selections, cite that selected file; for a directory selection, cite changed files beneath that directory. Other files may inform your research but are outside the selected diagram's validation scope.

## Build the shared diagram

Inspect before editing, especially when revising an existing draft:

```bash
codescope agent . diagram inspect
codescope agent . diagram schema
```

`diagram schema` returns the exact `edit_visualization` and `inspect_visualization` schemas used by CodeScope's internal harness. Apply one `edit_visualization` command at a time:

```bash
codescope agent . diagram edit '{"op":"set_intent","intent":"Show how the request crosses the queue boundary."}'
codescope agent . diagram edit '{"op":"create_form","form_id":"main","kind":"sequence"}'
codescope agent . diagram edit '{"op":"create_node","form_id":"main","node":{"id":"n1","label":"Queue request","detail":"Adds the request to the worker queue","code_refs":[{"file":"src/api.rs","hunk":0,"side":"new","start_line":42,"end_line":43}],"change":"added"}}'
```

Edits are synchronous. Read `result.accepted`, `result.error`, `result.revision`, and `result.draft` before issuing the next edit. Use stable form/node ids from that draft rather than guessing them.

Prefer the smallest visual that makes the changed behavior clearer: normally one form and three or four decisive boxes, or exactly two flat states for `before_after`. Use `sequence` plus `flows_to` only for chronology implemented by the selected code. Use specific edge labels. Omit `entity` for conceptual nodes; include it only when the exact file and symbol identity appears in live context.

When the draft is complete:

```bash
codescope agent . diagram finish
```

Finish is synchronous. Success returns `result.published: true` with the validated plan. On rejection, use `result.error`, `result.validation`, and `result.draft` to make only the needed correction, then finish again. If the epoch or selection changed during research, stop and read `context` before editing further.

Use `codescope agent . refresh` only when the user wants CodeScope to rescan Git and analysis state. It remains asynchronous; observe `result.live.refreshing` and `result.live.epoch` through `context`.
