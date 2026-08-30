# Review 02 — LSP lifecycle & capabilities (`codescope-lsp`)

Scope: initialize handshake, shutdown→exit→kill teardown, feature gating, utf-16↔utf-8
conversion (emoji/astral), overlay close+reopen, `client.rs` reader/writer/pending-id
machinery, and the `Content-Length` framing decoder. Method: read every module in
`crates/codescope-lsp`, traced the consumers (`codescope-analysis/src/engine.rs`,
`crates/codescope/src/main.rs`, `dispatcher.rs`), ran `cargo test -p codescope-lsp`
(51 unit tests + live `gopls_end_to_end`, all green), and verified the two headline
defects empirically against a live gopls with a scratch out-of-tree probe (no project
files were modified).

## Summary

The transport layer is solid: framing recovers from garbage, request-id matching handles
out-of-order and late responses, server→client requests get safe replies, teardown of the
process itself is correct, and the utf-16↔utf-8 conversion helpers are correct including
surrogate-pair snapping (verified with emoji round-trips). The defects are concentrated
one level up, in `gopls.rs`'s overlay/text plumbing: **position conversion is run against
the wrong document text** in the base-overlay path (confirmed live: a symbol at base
utf-8 col 28 comes back as col 22 — the raw utf-16 value; for a deleted file every column
collapses to 0), and **the deleted-file restore path reopens the vanished file as an empty
overlay** instead of closing it (confirmed live: gopls then pushes a phantom
`expected ';', found 'EOF'` error for a file that no longer exists, and the empty overlay
pollutes the package for the rest of the session). Both results are wrapped in
`Evidence::complete` — silent corruption of the honesty layer. Separately, the carefully
built graceful teardown is dead code in the shipped binary: `main.rs` aborts the
dispatcher task, so gopls is SIGKILLed via `kill_on_drop` on every exit.

Empirical probe output (live gopls, base line `const ( A = "😀😀😀"; B = 2 )`, worktree
line ASCII):

```
expected utf-8 col of B in base line = 28; utf-16 wire col = 22
BASE  sym B  sel=(2, 22)..(2, 23)     <- raw utf-16 col leaked through (should be 28)
GONE  sym A  sel=(2, 0)..(2, 0)       <- deleted file: all columns zeroed
GONE  sym B  sel=(2, 0)..(2, 0)
GONE  diagnostics after restore: 1
  -> Error expected ';', found 'EOF'  <- phantom diagnostic for a nonexistent file
```

## Findings

### F1 — Base-overlay symbol positions are converted with the wrong text

- **Severity**: high
- **Where**: `crates/codescope-lsp/src/gopls.rs:284` (root cause), reached from
  `base_document_symbols` at `gopls.rs:251-275`; contrast `document_symbols` at
  `gopls.rs:245`.
- **What**: `base_document_symbols(file, content)` opens `content` (the base revision) as
  the overlay and queries `textDocument/documentSymbol` — so every wire position refers to
  `content`. But `symbol_tree` (`gopls.rs:284`) does
  `std::fs::read_to_string(abs).unwrap_or_default()` and converts the utf-16 wire columns
  against the **current worktree** lines (which by definition differ from the base — that
  is why a base tree was requested). For a deleted file the read fails, text becomes `""`,
  `line_at` yields `""` for every line, and `utf16_col_to_utf8("", col)` returns 0 — every
  `range`/`selection` column in the base tree becomes 0. Both cases confirmed live (probe
  above). The result is returned as `Evidence::complete` (`gopls.rs:293`).
- **Why it matters**: base trees exist precisely for deletion mapping and symbol
  add/remove detection (architecture decision 3; `engine.rs:207-217`). v0 hunk mapping is
  line-granular so it mostly survives, but the corrupted columns flow into
  `SymbolTree::sort_recursive` ordering, digest/UI ranges, and any future position-based
  base query — silently, with completeness claimed. It also breaks the crate's central
  invariant ("all conversion happens at the wire boundary" — against the text the server
  actually holds, `lib.rs:11-13`).
- **Fix**: make `symbol_tree` take the document text as a parameter: worktree callers pass
  the string already read in `sync_worktree` (see F4), `base_document_symbols` passes
  `content`. ~5 lines, no API change.

### F2 — Deleted-file restore reopens an empty overlay instead of closing

- **Severity**: high
- **Where**: `crates/codescope-lsp/src/gopls.rs:258` + `gopls.rs:271` (`reopen(&abs, &disk)`
  with `disk = std::fs::read_to_string(&abs).unwrap_or_default()`).
- **What**: after the base query, the "restore the worktree view" step reopens the URI
  with the disk content. When the file does not exist in the worktree (a **pure deletion**
  — the primary reason base overlays exist), `disk` is `""`, so gopls is left holding an
  open, empty `.go` document for a path that has no file, for the rest of the session.
  Confirmed live: gopls immediately publishes `Error: expected ';', found 'EOF'` for the
  nonexistent file, which lands in the client's diagnostics cache (`client.rs:150-163`)
  and is retrievable via `diagnostics()`/`diagnostic_uris()`. An empty file that is part
  of an existing package can additionally degrade gopls's type-checking of sibling files
  in that package for subsequent worktree queries.
- **Why it matters**: phantom errors for deleted files and cross-file analysis
  contamination, persisting until process exit; `versions` also permanently records the
  ghost document (`gopls.rs:178`).
- **Fix**: in the restore step, if the worktree file is absent (or unreadable), send
  `textDocument/didClose` and remove the `versions` entry instead of `reopen(abs, "")`.

### F3 — Graceful teardown is never exercised by the binary; gopls is SIGKILLed on exit

- **Severity**: medium
- **Where**: `crates/codescope/src/main.rs:103` (`disp_handle.abort()`); teardown
  implementation `crates/codescope-lsp/src/client.rs:453-502`, `gopls.rs:647-649`,
  `service.rs:153-156`.
- **What**: `LspClient::shutdown` correctly implements shutdown request → exit
  notification → close stdin → 5 s grace (`SHUTDOWN_GRACE`, `client.rs:41`) → kill, and
  the live test proves it works. But no production code path calls it: the dispatcher owns
  the `LanguageService` and `main` simply aborts the dispatcher task, so the `Child` is
  dropped and tokio's `kill_on_drop` (`client.rs:266`) SIGKILLs gopls on every app exit.
  The documented lifecycle (`lib.rs:8-10`, architecture "graceful shutdown") is dead code
  in the app.
- **Why it matters**: gopls gets no chance to flush its state/caches; on slower teardowns
  the kill races runtime shutdown. The whole 60-line teardown path plus its 5 s grace
  logic is only reachable from tests, which is exactly how regressions hide.
- **Fix**: wiring-level (binary crate): have `dispatcher::run` shut down the engine's
  service when its loop ends, and replace the bare `abort()` with a close-signal +
  bounded await of the dispatcher. No `codescope-lsp` change needed.

### F4 — Worktree text is re-read per query step (TOCTOU between overlay and conversion)

- **Severity**: medium
- **Where**: `crates/codescope-lsp/src/gopls.rs:144` (read in `sync_worktree`) vs
  `gopls.rs:284` (`symbol_tree` re-read); same pattern at `gopls.rs:364`
  (`references`), `gopls.rs:465` (`implementations`), `gopls.rs:518` / `gopls.rs:545`
  (`prepare_call_hierarchy` / `prepare_type_hierarchy`), and `gopls.rs:571`
  (`location_from_wire` reads target files at conversion time).
- **What**: each query reads the file once to build the overlay and again (sometimes a
  third time) to convert positions. If the file changes on disk between the reads (save
  mid-analysis — the app's file watcher triggers on exactly that), the wire positions
  refer to the overlay text while the conversion uses the newer disk text; columns on
  changed non-ASCII lines are then wrong, same failure shape as F1 but transient.
- **Why it matters**: races with the editor are codescope's normal operating environment;
  the failure is silent and self-heals only on the next refresh.
- **Fix**: return the text from `sync_worktree` and thread it through the request/convert
  steps (fits naturally with the F1 fix). `location_from_wire`'s per-target reads are
  acceptable (worktree files at rest) but should be noted as best-effort.

### F5 — Cached push diagnostics are converted against whatever is on disk *now*

- **Severity**: medium
- **Where**: `crates/codescope-lsp/src/gopls.rs:184-197` (`diagnostics()`), cache fill at
  `client.rs:150-163`.
- **What**: gopls publishes diagnostics asynchronously for the document state it last
  analyzed — which, given `base_document_symbols`'s overlay flip (`gopls.rs:260-271`), can
  be the *base* content, an older worktree state, or F2's phantom empty file. `diagnostics()`
  converts those cached utf-16 ranges with a fresh `read_to_string(abs)`. There is no
  version/text tagging, so column conversion (and even line numbers) can be computed
  against text the server never saw. Consumers make it worse: `engine.rs:136` reads the
  cache immediately after opening the document, i.e. usually before gopls has published
  anything for that version.
- **Why it matters**: wrong or stale diagnostic positions in the snapshot; combined with
  F2 it can show errors for deleted files.
- **Fix**: cache the publish alongside the version/text it was published for (gopls echoes
  `version` in `publishDiagnostics`), or convert at cache-write time using the overlay
  text current at that moment; document the remaining staleness as line-precision-only.

### F6 — gopls adapter never marks `PushDiagnostics` as Supported

- **Severity**: low
- **Where**: `crates/codescope-lsp/src/capabilities.rs:61-63` + `capabilities.rs:115`
  (contract: "server adapters that know their server pushes (gopls does, quirk 6) mark it
  `Supported` themselves") vs `gopls.rs:84-96` (`start` never calls
  `features.set(Feature::PushDiagnostics, …)`).
- **What**: the resolved `FeatureSet` reports `Unknown` for push diagnostics on a gopls
  session, contradicting the module's documented contract and research 01 quirk 6.
- **Why it matters**: nothing gates on it today (`diagnostics()` reads the cache without
  `require`), but the `FeatureSet` flows to the TUI/AI layers, which per the architecture
  grey out unsupported views — diagnostics would be presented as unavailable.
- **Fix**: one line in `GoplsService::start` after `resolve_features`.

### F7 — jsonrpc classification edge cases: `error: null` and non-integer ids

- **Severity**: low
- **Where**: `crates/codescope-lsp/src/jsonrpc.rs:97` and `jsonrpc.rs:22-27`.
- **What**: (a) `if let Some(err) = obj.get("error")` treats the non-conforming-but-seen
  `{"id":1,"result":…,"error":null}` shape as an error response (code 0, `<no message>`),
  discarding the result. (b) `RequestId::from_value` returns `None` for non-i64 numeric
  ids, so a server→client request with id `2.5` would be classified as a *notification*
  (`jsonrpc.rs:87-95`) and never answered — a pathological server would hang; a response
  with `id: null` (spec-legal for unparseable-request errors) is dropped as
  unclassifiable, which is fine but worth knowing.
- **Why it matters**: gopls emits none of these; this only bites when the "generic client"
  claim is cashed in for other servers.
- **Fix**: (a) require `error` to be non-null, else fall through to `result`;
  (b) answer non-integer-id requests with `-32600`.

### F8 — Reader answers server→client requests inline; writer stall can stall dispatch

- **Severity**: low
- **Where**: `crates/codescope-lsp/src/client.rs:525` (`handle_message(...).await` in
  `reader_loop`) → `client.rs:119-146` (`answer_server_request` → `send_frame` under the
  writer mutex).
- **What**: replies to server requests are written from the reader task. If the child's
  stdin pipe is full (server wedged, not reading), `write_all` blocks the reader loop, so
  stdout also stops draining — a mutual pipe stall until the request timeout fires
  (requests then fail, but the session is effectively dead without a clear "server wedged"
  signal). gopls reads stdin continuously, so this is a robustness note, not an active bug.
- **Fix**: route outbound replies through a queue/writer task, or `try_send` with a small
  timeout in the reader path.

### F9 — Pending-entry race on server death; `notify` skips the alive check

- **Severity**: low
- **Where**: `crates/codescope-lsp/src/client.rs:386-397` vs `client.rs:532-533`;
  `client.rs:442-446`.
- **What**: if the reader loop exits (server crash → `fail_all_pending`) between
  `request()`'s `is_alive()` check and the `pending.insert`, the new entry is failed by
  nobody and the caller waits out the full timeout instead of failing fast with
  `ServerExited`. Bounded by the timeout, so degradation only. Relatedly, `notify()` never
  checks `alive`, so post-crash notifications surface as `LspError::Io(BrokenPipe)` (or
  silently succeed into a dying pipe) rather than `ServerExited` — inconsistent error
  taxonomy for the same condition.
- **Fix**: re-check `alive` after inserting (fail-and-remove if dead), and mirror the
  alive check in `notify`.

### F10 — Framing: oversized garbage clears a possibly-valid partial header; recovery can drop one frame

- **Severity**: low
- **Where**: `crates/codescope-lsp/src/framing.rs:55-61` (`self.buf.clear()`).
- **What**: when >16 KiB arrive with no `\r\n\r\n`, the decoder clears the whole buffer,
  including a trailing partial valid header (e.g. `…garbage…Content-Le`). The stream
  re-syncs on later frames via the `Content-Length:` marker path (`framing.rs:76-84`), but
  the frame whose header was truncated is lost, turning one response into a timeout.
  Requires 16 KiB of terminator-free garbage on stdout — pathological; the skip-and-recover
  design (never fatal) otherwise checks out, including the `pos > 0` progress guarantee.
- **Fix**: retain the last `MAX_HEADER_LEN`-window tail (or at least the last 3 bytes plus
  any trailing `Content-Length:` prefix candidate) instead of clearing everything.

### F11 — Module root may resolve above the git toplevel, silently breaking every path mapping

- **Severity**: low
- **Where**: `crates/codescope-lsp/src/gopls.rs:653-661` (`find_module_root` walks up
  without bound) with `gopls.rs:119-121` (`abs_path` joins repo-relative `FileId`s onto the
  *module* root) and `crates/codescope/src/main.rs:54,60` (called with the git toplevel).
- **What**: if the repo toplevel has no `go.mod` but some ancestor directory does (a git
  repo nested inside a Go module), gopls anchors outside the repo and `abs_path` computes
  `<ancestor>/<repo-relative-path>` for every query — wrong files or `FileRead` errors,
  with no diagnostic pointing at the cause. (The inverse layout — go.mod only in a
  subdirectory — cleanly degrades to git-only mode, which is a documented v0 limit.)
- **Fix**: stop the upward walk at the caller's root, or keep the repo root separately and
  use it for `abs_path`/`file_id`.

### F12 — Overlay lifecycle housekeeping: never-closed overlays; non-atomic base sequence

- **Severity**: low
- **Where**: `crates/codescope-lsp/src/gopls.rs:142-180` (`sync_worktree`/`reopen`,
  `versions` map), `gopls.rs:251-275` (overlay flip without a cross-call lock).
- **What**: (a) every file ever queried stays open in gopls until process exit — the
  `versions` map only grows; overlay content equals disk so it is a memory/refresh cost,
  not correctness. (b) the base-overlay sequence (reopen base → query → restore) holds the
  `versions` lock only inside each `reopen`, so two concurrent callers could interleave
  overlay states. Today the engine (`engine.rs:133`, sequential per-file loop) and the
  single dispatcher actor serialize everything, so this is latent — but nothing in
  `GoplsService`'s `&self` API enforces it.
- **Fix**: document the single-caller requirement on `GoplsService`, or hold an async
  per-service (or per-file) lock across `base_document_symbols`; optionally didClose
  overlays after analysis of a changeset completes.

## Verified-good (no findings)

- **utf-16 conversion incl. emoji**: `encoding.rs:57-80` snapping semantics (mid-surrogate
  and mid-utf-8 offsets snap to the end of the character) are documented, tested
  (`encoding.rs:148-169`), and round-trip on all char boundaries; the live probe shows
  correct wire cols (22 utf-16 for byte col 28) — the F1 defect is text selection, not the
  converters.
- **Handshake**: init params match research 01 recommendation 7 (rootUri +
  workspaceFolders + hierarchicalDocumentSymbolSupport + `general.positionEncodings`);
  `positionEncoding` is read from the right response field with utf-16 default and a
  reasoned utf-32 refusal (`encoding.rs:33-51`); broken-session (all-null) detection
  matches quirk 5 (`capabilities.rs:64-82`) and correctly fails `start` before
  `initialized` is sent; capability shape tolerance (bool/object/null/bare-int sync)
  is tested against all researched server shapes.
- **Feature gating**: every public query calls `require()` before any wire traffic
  (`gopls.rs:234,256,362,397,430,463,489`); `Unknown` gates as unsupported
  (`relation.rs` `is_supported`), satisfying "never send requests the server didn't
  advertise".
- **Pending-id machinery**: ids are process-unique (`AtomicI64` fetch_add,
  `client.rs:391`); out-of-order, late-after-timeout, error-code mapping (-32601 →
  `MethodNotFound`), and crash-fails-pending paths all behave and are unit-tested
  (`client.rs:623-849`).
- **Teardown mechanics** (when invoked): shutdown request failure does not block exit;
  stdin close, cancel token, pending flush, 5 s grace then kill, reader/stderr task
  aborts — ordered correctly (`client.rs:453-502`); uncooperative-server outcome is
  `Killed`, never an error.
- **Diagnostics cache URI keys**: probed live with a `héllo test.go` (space + non-ASCII)
  — gopls's published URI matches `uri_from_path`'s encoding, so exact-string cache
  lookups (`client.rs:69`, `gopls.rs:189`) hold on this platform.

## Verdict

**fix-first.** The transport, framing, encoding, gating, and teardown machinery are
well-built and well-tested, but F1 and F2 are confirmed-live correctness defects in the
base-overlay path — the exact feature the architecture introduced overlays for — and both
corrupt data silently under an `Evidence::complete` label. Their fixes are small and local
(thread the right text into `symbol_tree`; didClose on restore of missing files) and F4/F6
fall out of the same patch. F3 (graceful shutdown dead in the binary) should be fixed in
the wiring before ship but does not block the lsp crate itself.
