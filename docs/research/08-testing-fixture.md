# 08 — Testing strategy & Go fixture design

Scope: codescope (Rust/Ratatui/crossterm/tokio; gopls first; AI optional). All tool
claims below were verified locally: gopls v0.21.0, git 2.50.1, go 1.26.7, macOS arm64.

## 1. Go fixture repo

### 1.1 Layout (package-per-layer, cross-package call chain)

```
testdata/go-fixture/                # canonical fixture, REGENERATED, never hand-edited
  go.mod                            # module example.com/codescope-fixture; go 1.26
  README.md
  cmd/server/main.go                # wires repo->service->handler, calls h.GetUser(1)
  internal/store/store.go           # User struct; Repository IFACE; MemoryRepo impl; ErrNotFound
  internal/store/file.go            # FileRepo impl (2nd Repository impl)  [renamed, see below]
  internal/store/store_test.go      # TestMemoryRepoRoundTrip
  internal/service/service.go       # UserService{repo store.Repository}; FindUser, RenameUser
  internal/service/service_test.go  # TestFindUser (uses MemoryRepo)
  internal/api/api.go               # Handler{svc}; GetUser -> svc.FindUser (top of call chain)
```

Semantics the fixture must exercise (all verified to work with gopls):
- **Cross-package definition**: `FindUser` call in api.go -> def in service/service.go (verified).
- **Interface with 2+ impls**: `textDocument/implementation` on `Repository` returns MemoryRepo
  (store.go) and FileRepo (file_repo.go) (verified).
- **Cross-package references incl. tests**: `references` on `Repository.Get` hits store.go,
  service.go, store_test.go (verified).
- **Struct passed between layers**: `store.User` flows api <- service <- store.

### 1.2 Git states the fixture must contain (all verified via `git status --porcelain`)

| State | How produced | Expected porcelain v1 |
|---|---|---|
| committed base on `main` | initial commit | (clean for those files) |
| branch diverges from main | `feature/pagination` + commit adding `internal/store/pagination.go` | `git diff main..feature/pagination` = 1 file |
| staged | `internal/service/list.go` written, `git add`, no commit | `A  internal/service/list.go` |
| unstaged | append `UnstagedDraft` to api.go, no add | ` M internal/api/api.go` |
| untracked | `tools/scratch/scratch.go` (own dir, `//go:build ignore`) | `?? tools/` |
| renamed | `git mv internal/store/file.go internal/store/file_repo.go` | `R  internal/store/file.go -> internal/store/file_repo.go` |

### 1.3 Regeneration script (store at `testdata/go-fixture/regenerate.sh`; CI + tests call it)

```bash
#!/usr/bin/env bash
# regenerate.sh <target-dir> — rebuilds the fixture deterministically.
set -euo pipefail
DIR="${1:?usage}"; rm -rf "$DIR"; mkdir -p "$DIR"; cd "$DIR"
export GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z"
git init -q -b main .; git config user.email f@fixture; git config user.name fixture
git config commit.gpgsign false
# ... heredocs writing every file from section 1.1, then:
git add -A; git commit -qm "base: store/service/api layers with Repository interface"
git switch -qc feature/pagination   # write pagination.go; add; commit -qm "feature: add Paginate"
git switch -q main
# staged:    write internal/service/list.go;  git add internal/service/list.go
# unstaged:  append UnstagedDraft func to internal/api/api.go   (no git add)
# untracked: mkdir tools/scratch; write scratch.go with //go:build ignore + package main
# renamed:   git mv internal/store/file.go internal/store/file_repo.go
go build ./...                      # sanity: fixture must typecheck for gopls
```

- **Determinism**: fixed `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` yields identical commit OIDs
  across runs/machines (verified: two independent builds -> same OID). Snapshot tests may then
  assert on OIDs; still prefer asserting on paths/status letters.
- Full script (~150 lines with all heredocs) lives in-repo; the doc keeps the skeleton.

### 1.4 Git pitfalls (verified)

- **Untracked dirs collapse**: plain `git status --porcelain` reports `?? tools/`, not the file.
  Always pass `-uall` (verified: `?? tools/scratch/scratch.go`).
- **Rename detection is index-only**: a worktree rename is seen as delete+untracked; the fixture
  must use `git mv` (or staged rm+add) to get an `R` entry. Worktree-side rename similarity is
  not computed by `git status`.
- **Parse porcelain v2, not v1**: v2 (`git status --porcelain=v2 --branch -uall`) is stable
  machine output; renames are `2 R. ... R100 new\told` (tab-separated — split on TAB first).
- Keep the untracked file **valid Go in its own directory with `//go:build ignore`**; an invalid
  or root-level `package main` file breaks `go build ./...` and degrades gopls (verified).

## 2. LSP integration tests (real gopls)

- **Location**: `tests/lsp/`, each test copies the fixture: `let tmp = tempfile::tempdir()` +
  copy (or run `regenerate.sh` into it once per process via `std::sync::Once`, then copy per
  test). Never let tests mutate the canonical fixture; `tempfile 3.27` handles cleanup.
- **Prereq + skip**: helper `fn gopls() -> Option<PathBuf>` runs `gopls version`; on failure
  `eprintln!("SKIP: gopls not found"); return;` (env-skip pattern, not `#[ignore]`, so CI with
  gopls installed runs them by default). Name these tests `gopls_*`.
- **Spawn**: `gopls serve` over stdio (default; verified). Add `-logfile=<tmp>/gopls.log` for
  debugging flakes. `rootUri` = temp copy.
- **Handshake protocol (verified)**: `initialize` -> response -> send `initialized`
  notification -> requests. Responses must be **matched by `id`** — gopls interleaves
  server->client notifications (`window/logMessage`, `$/progress`) before/around responses.
- **Measured warm timings** (this machine): init 0.05 s; first `definition` 0.38 s (includes
  package load); `implementation`/`references` <0.05 s. **Timeouts**: first semantic request
  60 s (cold module cache on CI can be minutes), steady-state 10 s, shutdown 5 s.
- **Shutdown (verified)**: send `shutdown` request, await response, then send `exit`
  notification; process stays alive between the two; exit code 0. On shutdown timeout:
  escalate to `child.kill()`. Test both paths (fake server that ignores shutdown -> client
  must SIGKILL within deadline).
- **gopls error behavior (verified)**: unknown method -> `-32601`; any request after
  `shutdown` -> error code `0` "session is shut down"; hover on unopened file -> `null`
  result (tolerant, but still send `didOpen` with worktree text so overlays match disk).
- Cheap CLI oracle for cross-checks: `gopls definition <file:line:col>`.

## 3. Fake servers & providers (no network)

- **Fake LSP server** (unit/negative tests): hand-rolled stdio JSON-RPC in test code (write
  `Content-Length` frames; ~80 lines, no dep; `lsp-server 0.10` also fine). Scriptable replies:
  (a) valid, (b) malformed JSON body, (c) wrong `Content-Length`/truncated stream, (d) `id`
  mismatch, (e) `-32601` error, (f) `initialize` result **missing** `definitionProvider` etc.
  Client must degrade per-capability, never panic on (b)-(d).
- **Fake AI provider**: `ScriptedProvider` implementing the provider trait over
  `VecDeque<FakeResponse>`; variants: `ValidPlan(json)`, `MalformedJson("{...")`,
  `HallucinatedEntities` (symbols not in impact graph -> must be dropped + warned),
  `StaleEpoch { epoch }` (plan tagged with old epoch -> discarded after a newer change),
  `Latency(duration)`. Assert provider-neutral core works with zero providers configured.
- **Live AI test**: `#[ignore] + env CODESCOPE_LIVE=1 + provider key env; run manually via
  `cargo test -- --ignored live_ai_smoke`.

## 4. App-level hardening tests

- **Terminal restoration**: own `TerminalGuard` (Drop -> `disable_raw_mode` +
  `LeaveAlternateScreen`) + panic hook calling restore before the default hook. Ratatui 0.30
  `ratatui::init/restore` exist but keep the guard for panic paths. Test: spawn the built
  binary in a pty (`portable-pty 0.9`), make it panic (hidden `CODESCOPE_TEST_PANIC=1` arm),
  assert pty output contains leave-alternate-screen `\x1b[?1049l` and raw-mode teardown, and
  exit code != 0. Rendering itself: `ratatui::backend::TestBackend` + `insta 1.48` snapshots
  (redact temp paths).
- **Epoch coalescing**: rapid `didChange` v2..v6 within the debounce window; assert exactly one
  re-analysis at latest epoch and that stale results are dropped. Drive clocks with
  `tokio::time::{pause,advance}` (feature `test-util`) — no real sleeps.
- **Unsupported capabilities**: initialize against fake server with minimal caps; assert UI
  shows "unavailable" affordances instead of errors.

## 5. CI gating & naming

| Test class | Name/attr | Needs | CI behavior |
|---|---|---|---|
| unit | default | nothing | always run |
| fixture/git | default | git | always run (script regenerates) |
| gopls integration | `gopls_*`, env-skip helper | gopls binary | job installs gopls -> runs; local without -> skips |
| TUI pty | `pty_*` | portable-pty | run on linux+macos |
| live AI | `live_*`, `#[ignore]` + `CODESCOPE_LIVE=1` | network+key | manual only |

Dev-deps: `tempfile 3.27`, `assert_cmd 2.2` + `predicates 3.1` (CLI), `insta 1.48` (snapshots),
`rstest 0.26` (parametrized fixture states), `portable-pty 0.9`, tokio `test-util`.

## Recommended decisions

1. Canonical fixture generated by `testdata/go-fixture/regenerate.sh` with fixed git dates
   (deterministic OIDs); tests copy it into `tempfile` dirs, never mutate it.
2. Git layer parses `git status --porcelain=v2 --branch -uall` only; rename via `git mv`.
3. gopls tests use env-skip (not `#[ignore]`), 60 s first-request / 10 s steady-state /
   5 s shutdown-then-kill timeouts, response matching by `id`, `shutdown`+`exit` protocol.
4. One hand-rolled fake LSP server + one `ScriptedProvider` fake AI cover malformed JSON,
   hallucinated entities, stale epochs; live AI behind `#[ignore]` + `CODESCOPE_LIVE=1`.
5. `TerminalGuard` RAII + panic hook, verified through a pty subprocess test; debounce/epoch
   tests use `tokio::time::pause` for determinism.
