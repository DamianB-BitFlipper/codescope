# 06 — Reactive Update Architecture

How codescope wires file/git/LSP change detection, cancellation, progressive
loading, and channels under tokio. All crate versions and git behaviors below
were verified locally (crates.io API, git 2.50 experiments, docs.rs).

## 1. Crates (verified against crates.io)

| crate | version | features / notes |
|---|---|---|
| `notify` | 8.2.0 | default features → `macos_fsevent` on macOS (`macos_kqueue` opt-in) |
| `notify-debouncer-mini` | 0.7.0 | matches notify 8.x; default passthrough `macos_fsevent` |
| `tokio` | 1.53.x | `rt-multi-thread`, `macros`, `sync`, `time`, `fs` |
| `tokio-util` | 0.7.19 | `sync::CancellationToken` |
| `crossterm` | 0.29.0 | `features = ["event-stream"]` (verified: pulls `futures-core`+`events`) |
| `ratatui` | 0.30.2 | |
| `futures` / `tokio-stream` | 0.3 / 0.1 | `StreamExt` for `crossterm::event::EventStream` |

On macOS, FSEvents is the right backend: one watch per directory tree, no
per-file fds (kqueue exhausts fds on large repos). notify 8 defaults to it.

## 2. File watching and debouncing

- One `notify::RecommendedWatcher` for the **working tree** (recursive), one
  for the **git dir** (recursive). Resolve the git dir with
  `git rev-parse --git-dir` / `--git-common-dir`, never assume `.git/`:
  in worktrees `.git` is a file (`gitdir: <path>`); per-worktree HEAD/index
  live in the gitdir, shared refs in the common dir (verified locally).
- Debounce with `notify-debouncer-mini` 0.7. Verified API:
  `new_debouncer(Duration, handler) -> Debouncer<RecommendedWatcher>`,
  handler receives `DebounceEventResult = Result<Vec<DebouncedEvent>, Error>`,
  `DebouncedEvent { path: PathBuf, kind: DebouncedEventKind::{Any, AnyContinuous} }`
  (kind enum is `#[non_exhaustive]` — match with a wildcard arm).
  `-full` adds rename tracking we do not need.
- The notify handler runs on a watcher thread: forward into tokio with
  `mpsc::UnboundedSender` or `try_send` + drop-counter. Never `block_on` there.
- Debounce windows: **300 ms** for working-tree edits, **100 ms** for git-dir
  events (git ops are discrete; fast feedback on branch switch matters).
- FSEvents is *lossy* under load (`MustScanSubDirs`, event overflow). Add a
  slow safety net: every ~30 s and on terminal focus-gained, compare
  HEAD/index mtimes + working-tree scan watermark; reconcile if diverged.

### What to watch inside the git dir (verified mtime experiments, git 2.50)

| operation | `.git/HEAD` | `.git/index` | `refs/heads/*` | `logs/HEAD` |
|---|---|---|---|---|
| edit working file only | — | — | — | — |
| `git add` | — | ✓ | — | — |
| `git commit` | — | ✓ | ✓ | ✓ |
| `git checkout -b` / switch | ✓ | ✓ | — | ✓ |

Consequences:
- **Watching only `.git` misses plain edits; watching only the tree misses
  staging/commit/branch changes. You need both watches.**
- Trigger a status refresh on: `HEAD`, `index`, `refs/`, `packed-refs`
  (appears after `git gc`), plus `MERGE_HEAD`/`FETCH_HEAD`/`ORIG_HEAD` for
  merge/fetch state. Filter out `objects/` and `logs/` noise by path prefix.
- Polling `git status` on a timer is unnecessary: event-driven refresh +
  the 30 s safety poll above. A warm `git status` is cheap (index stat
  cache), so spawning `git status --porcelain=v2 -z` per refresh is fine;
  `git2`/`gix` in-process is a later optimization, not a requirement.

## 3. Epoch-based supersede + cancellation

- Shared `Arc<AppState>` holds `epoch: AtomicU64`. The dispatcher (§5) is the
  **only** writer: it bumps epoch once per accepted debounced change-set
  (one fs batch or one git-state change = one bump, not one per file).
- Every spawned job (LS refresh, graph recompute, AI request) captures
  `epoch_at_start`. Before mutating state or sending results to the UI it
  checks `state.epoch.load() == epoch_at_start`; stale results are dropped.
  **The epoch check at apply time is the correctness mechanism.**
- `tokio_util::sync::CancellationToken` is the *optimization*: one token per
  subsystem job, stored in the dispatcher; superseding a job calls
  `cancel()` on the old token. Long jobs `select!` on `token.cancelled()`
  at await points. Use `child_token()` for sub-steps (e.g., per-file LS
  refreshes under one graph recompute).
- Pitfall: in-flight LSP requests to gopls are not cancelled by dropping the
  future on our side unless we also send `$/cancelRequest`. Simpler and
  sufficient: let them finish, drop stale results by epoch. Send
  `$/cancelRequest` only if profiling shows wasted gopls CPU.
- Pitfall: don't bump the epoch for UI-only state (cursor moves, panel
  focus) — lazy queries (§4) key off cursor position, not repo epoch.

## 4. Progressive loading

| tier | what | when |
|---|---|---|
| T0 | git status/diff summary, branch, ahead/behind | immediately at startup and per git event; <100 ms, spawn `git` on a blocking thread or use `tokio::process` |
| T1 | gopls `initialize` + workspace load | background task at startup; UI shows "indexing…" indicator; gopls reports progress via `$/progress` (workDoneProgress) |
| T2 | semantics of **changed files first**: didOpen/didChange + documentSymbol for files from git status | as soon as gopls is ready |
| T3 | callers/callees (`callHierarchy/incomingCalls` etc.) for the **selected** symbol only | lazy, on cursor/selection change, debounced 200 ms, cache-first |

Caching and invalidation:
- Per-file artifacts (symbols, diagnostics summary) keyed by
  `(path, content_hash)` — xxhash3/blake3 of file bytes. This cache **survives
  epoch bumps**: an unrelated edit does not re-analyze untouched files.
- Graph-level aggregates (impact graph, semantic change-set) keyed by epoch;
  recomputed from the per-file cache, not from gopls, on bump.
- gopls itself holds the source of truth for open/unsaved buffers; on epoch
  bump send `didChangeWatchedFiles` (or `didChange` for open buffers) only
  for files whose content hash actually changed.

## 5. Channel topology (recommended: fan-in actor, not one bus)

```
notify debouncer(s) ──mpsc(unbounded)──▶ ┐
git safety poller ────mpsc(unbounded)──▶ ├─▶ DISPATCHER task (owns AppState,
gopls notifications ──mpsc(unbounded)──▶ ┤    bumps epoch, starts/supersedes
AI responses ────────mpsc(unbounded)───▶ ┘    jobs, epoch-checks applies)
                                              │ watch::Sender<UiSnapshot>
                                              ▼
TUI loop: select! { biased;                UI only ever needs the *latest*
  ev = EventStream.next()  (crossterm)     snapshot → tokio::sync::watch,
  _ = snapshot_rx.changed()                not a queued mpsc.
  _ = frame_tick.tick()  (~33 ms)
}
```

- **Per-subsystem unbounded mpsc into one dispatcher task**, consumed in a
  single `select!` loop. Unbounded is safe here because producers are
  already debounced/coalesced; add a `try_send`+counter on the fs path to
  log drops if it ever grows.
- Reject the single broadcast bus: `tokio::sync::broadcast` forces every
  consumer to filter every message and surfaces `Lagged` errors to slow
  consumers; an mpsc fan-in + actor has neither problem and gives one place
  that owns epoch discipline.
- Reject one shared `mpsc<AppEvent>` for everything *into* the UI: it mixes
  latest-value state (snapshots) with commands. `watch` is the right
  primitive for state; mpsc for genuine event streams into the dispatcher.
- TUI loop: `crossterm::event::EventStream` (feature `event-stream`,
  verified) + `futures::StreamExt`; `select!` with `biased;` so keyboard
  input is handled before repaints. Render only when the snapshot changed
  or a ~33 ms animation tick fires; never render per input event blindly.
- All channel sends from non-async contexts (notify handler thread) use
  unbounded send; all async-side sends use unbounded or bounded(≥64) —
  never a small bounded channel on the UI path where backpressure could
  deadlock the dispatcher against a busy renderer.

## 6. Backpressure and AI gating

Debounce chain (each stage only fires if the previous produced real change):

| stage | window | trigger out |
|---|---|---|
| fs events | 300 ms (notify-debouncer-mini) | change-set batch |
| epoch bump | immediate per batch | new epoch |
| LS refresh | coalesce didChange bursts 300–500 ms | updated per-file cache entries |
| semantic delta | immediate after LS apply | recompute impacted-symbol set |
| AI request | **≥1.5–2 s trailing quiet AND min-interval ≥5 s** | at most one in-flight request |

- AI fires only when the **semantic change-set delta** is non-empty: hash the
  sorted (symbol, relation) set of the impact graph after LS refresh; compare
  with the hash of the last AI-sent set. Keystrokes that don't move the
  graph (comments, whitespace, unfinished identifiers that fail to parse
  identically) produce zero AI traffic.
- Hard gates on top: AI disabled ⇒ never send; repo epoch changed while
  request in flight ⇒ cancel token + drop response; one in-flight max
  (supersede, never queue).
- Backpressure rules: producers coalesce (debouncer, keep-latest `watch`),
  consumers never block producers, and any queue that can grow unboundedly
  gets a length metric + warn log.

## 7. Recommended decisions

1. `notify` 8.2 (FSEvents default) + `notify-debouncer-mini` 0.7 at 300 ms
   (tree) / 100 ms (git dir). Skip `-full`; `DebouncedEventKind` is
   `#[non_exhaustive]` — use a wildcard arm.
2. Watch both the working tree and the resolved git dir(s)
   (`rev-parse --git-dir`/`--git-common-dir`); refresh git state on
   HEAD/index/refs/packed-refs events; 30 s safety poll + focus-gained check
   because FSEvents is lossy. No fixed-interval `git status` polling.
3. Single-writer `AtomicU64` epoch in the dispatcher; epoch check at apply
   time is mandatory; `CancellationToken` per subsystem job for early abort;
   don't bother with LSP `$/cancelRequest` initially — drop stale results.
4. Two-level cache: per-file `(path, content_hash)` survives epochs;
   graph aggregates keyed by epoch and rebuilt from the per-file cache.
5. Topology: per-subsystem unbounded mpsc → one dispatcher actor →
   `watch::channel<UiSnapshot>` → TUI. `select! { biased; }` over crossterm
   `EventStream`, `snapshot.changed()`, 33 ms frame tick.
6. AI: trailing 1.5–2 s quiet + ≥5 s min interval + semantic-set-hash delta
   gate + one in-flight superseding request. App fully functional with the
   AI stage absent.
7. Load order: git info first frame → gopls init in background with a
   progress indicator → changed-file semantics → lazy call hierarchy on
   selection (200 ms debounce).
