# Local telemetry contract

Codescope writes always-on telemetry to a `telemetry/` directory beside its global configuration.
Every process/session creates a distinct `<timestamp>-<pid>-<nonce>.jsonl` file, preventing one
unbounded global log. The directory is owner-only and each append-only stream is
owner-readable/owner-writable on Unix. Telemetry is local only and has no upload path. Each line is
one independently valid JSON object with session, sequence, elapsed-time, repository, `origin`,
event, and data fields. `origin` is always one of `application`, `user`, `internal_agent`, or
`external_agent`; consumers never need to infer provenance from an event name or payload string.

## Diff snapshots

When the dispatcher accepts `ChangesetReady` for the current epoch, it builds a `diff.snapshot`
from the exact unified sections retained by the Git parser that produced the UI's `ChangeSet`.
Telemetry never runs a second Git command. `data.payload` contains:

- an opaque repository identity (the repository root itself is never stored);
- the Codescope comparison scope, fallback state, HEAD state/object, and resolved base/head sides;
- `canonical_diff`, the complete retained unified patch after path exclusions and secret
  scrubbing, without line or byte truncation; and
- changed-file status/path metadata plus hunk indices, old/new coordinates, section labels, and
  byte ranges into `canonical_diff`.

`diff_snapshot_id` is `sha256:` followed by the SHA-256 digest of the exact compact JSON encoding
stored under `data.payload`. Multiline and Unicode patch content is represented as an ordinary JSON
string and therefore round-trips through JSONL. A payload is written only once per session file;
unchanged refreshes in that session reuse its ID. A changed scope, base/head, or recorded diff
payload produces a different ID. A later session has its own self-contained copy so each file can
be analyzed or removed independently.

The active `diff_snapshot_id` is a top-level field on subsequent UI input, UI state/snapshot,
controller, and LLM records, allowing a trajectory to be joined to the precise code comparison it
used. Codescope clears the active ID as soon as an epoch refresh starts and emits
`diff.snapshot_unavailable`; records remain uncorrelated until a valid current comparison is
accepted. Provider work uses a request-scoped copy of the ID so a late response cannot be
misattributed to a newer comparison.

## Agent provenance and command correlation

Provider requests, responses, errors, and tool activity emitted by Codescope's built-in model use
`origin: "internal_agent"`. Direct keyboard and mouse events use `origin: "user"`.

Every `codescope agent` invocation creates structured `agent.command` records with
`origin: "external_agent"`. The short-lived CLI stream records client start and completion; the
running TUI stream records server receipt and completion for commands that connect to it, including
read-only `context` and `diff` calls. Their data contains:

- `command_id`, generated from the unique client telemetry session and shared across the socket;
- stable `operation` names such as `context`, `diff`, `focus`, `diagram.inspect`, `diagram.edit`,
  `diagram.finish`, and `refresh`;
- `side` (`client` or `server`), `phase`, and `status`; and
- the requested `view_id`, plus `result_view_id` when `context` captures a view.

Mutating commands additionally produce `input.control` in the TUI stream with the same
`command_id`, `operation`, and `view_id`, before/after UI state, and active `diff_snapshot_id`.
Thus an external command can be joined from its CLI invocation through socket handling to the
visible change it caused, while remaining unambiguously distinct from internal `llm.*` activity.

## Privacy boundary

Diff paths pass through Git ignore rules, `.codescopeignore`, and the compiled sensitive-file
denylist before inclusion. Retained patch text and metadata pass through repository-root redaction
and recognizable-secret scrubbing before hashing and storage. Excluded file contents and paths,
authorization headers, API keys, recognizable credential material, and absolute repository paths
are not recorded. Because hashing happens after these transformations, the ID verifies the payload
that is actually present in the stream rather than the pre-scrub source text.
