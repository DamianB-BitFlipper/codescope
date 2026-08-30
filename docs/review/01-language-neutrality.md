# Review 01 — Language neutrality

Reviewed against the claim set: core types are language-neutral; the `LanguageService` enum /
`GoplsService` split cleanly separates generic LSP from gopls specifics; analysis consumes a
neutral `SemanticSource`; adding rust-analyzer should not touch `codescope-git`,
`codescope-analysis`, or `codescope-tui`. Method: read every crate's sources, grep for
Go/gopls tokens outside tests, and walk through a hypothetical rust-analyzer addition
end to end. No code was modified.

## Summary

The type layer is genuinely language-neutral. `codescope-core` (`SymbolKind` covering the full
LSP 1–26 space with an `Unknown` default, `SymbolTree`/`SymbolRef`/`EntityRef`, `Feature`/
`FeatureSet`/`Availability`, `LsStatus`, `Evidence`) carries no Go semantics — Go appears only
in doc-comment examples and test data (`crates/codescope-core/src/semantic.rs:41`,
`crates/codescope-core/src/semantic.rs:521`). `codescope-git` has zero language awareness.
`codescope-lsp`'s transport (`client.rs`), capability resolution (`capabilities.rs` — tested
against pyright/tsls/rust-analyzer response shapes), and encoding conversion (`encoding.rs` —
utf-8/utf-16 per session, rust-analyzer's utf-8 already handled) are server-agnostic.
`codescope-analysis` consumes only the `SemanticSource` trait plus core types; its unit tests run
on a scripted source with no server. The AI prompt/digest/validator and the binary wiring are
neutral. Dependency direction is enforced in Cargo metadata: `codescope-tui` depends only on
`codescope-core`.

The gaps are concentrated in two places. First, one literal leak: the TUI top bar hardcodes the
string `gopls:` (`crates/codescope-tui/src/render.rs:109`), which falsifies "adding
rust-analyzer doesn't touch tui" as stated — trivially fixable. Second, the generic/adapter
split inside `codescope-lsp` is nominal rather than substantive: `GoplsService` (662 lines)
contains ~80–90% reusable LSP logic (overlay lifecycle, wire↔domain conversion, response
parsing, feature gating, all query methods), so a rust-analyzer adapter today means copying
~450 lines and re-tagging `languageId`. That is contained inside `codescope-lsp` (the
architecture's crate-boundary promise holds), but it is far from the research doc's
"per-server adapter ~200 lines". Additionally, there is no concept of file ownership: every
changed file — including READMEs and YAML — is sent to gopls as `languageId: "go"`.

## Findings

### 1. TUI hardcodes the server name "gopls" in the top bar
- **Severity**: medium
- **Where**: `crates/codescope-tui/src/render.rs:109` (`"  │  gopls: {}"`); contrast
  `crates/codescope-tui/src/snapshot.rs:25` (`pub ls: LsStatus` — status only, no server
  identity field).
- **What**: The top bar renders the literal label `gopls:` for the language-server status.
  `UiSnapshot` carries no server name, so the renderer cannot display anything else.
- **Why it matters**: This is the single concrete violation of "adding rust-analyzer shouldn't
  touch tui". With a `RustAnalyzer` variant, the UI would either lie ("gopls: ✓" while running
  rust-analyzer) or require a tui edit. It is also the only place outside `codescope-lsp`
  (excluding tests/doc comments) where the token `gopls` appears in shipping code.
- **Suggested fix**: Either render a generic label (`lsp:`/`lang:`), or add a display name to
  `UiSnapshot` (e.g. `ls_name: &'static str`/`String`) that the binary populates from
  `LanguageService`. One-line to few-line change.

### 2. The generic/adapter split puts ~80–90% generic LSP logic inside `GoplsService`
- **Severity**: medium
- **Where**: `crates/codescope-lsp/src/gopls.rs` — overlay lifecycle
  (`sync_worktree`/`reopen`, gopls.rs:142–186), position-conversion plumbing (gopls.rs:201–238),
  documentSymbol response parsing including the flat-`SymbolInformation` degraded fallback
  (`symbol_tree`, gopls.rs:277–345), all eight query methods with standard LSP request JSON,
  prepare helpers (gopls.rs:512–566), and wire→domain converters
  (`location_from_wire`/`call_item_to_ref`/`type_item_to_ref`/`goto_response_to_refs`,
  gopls.rs:568–651). Genuinely gopls-specific content is small: spawn command + `CODESCOPE_GOPLS`
  (gopls.rs:54–66), initialize params (gopls.rs:68–83), `languageId: "go"` (gopls.rs:171),
  `find_module_root` for `go.mod` (gopls.rs:653–661), and the first-request timeout tuning.
- **What**: `LspClient` is transport-only (framing, id matching, diagnostics cache, default
  server→client replies — genuinely generic and good). But everything between transport and the
  `LanguageService` enum lives in the gopls adapter, although almost none of it is
  gopls-specific. Research 01 promised "shared plumbing … in one generic LspClient
  (…position-encoding conversion, capability resolution); each server adapter only supplies
  spawn command, initialize params, capability mapping, enrichment hooks" (~200 lines).
  Capability resolution and encoding helpers do live in shared modules, but the adapter *calls*
  them from ~450 lines of otherwise generic orchestration.
- **Why it matters**: Adding rust-analyzer stays contained in `codescope-lsp` (the crate-level
  claim survives), but in practice it means copy-pasting the bulk of `gopls.rs` and keeping two
  copies of protocol logic in sync — the flat-symbol fallback, overlay-restore-on-error, and
  response-parsing edge cases would have to be fixed twice. That is the classic drift failure
  mode the enum-dispatch design was meant to avoid.
- **Suggested fix**: Extract a server-agnostic session type (client + root + encoding + open-doc
  versions + timeout policy) exposing the query/overlay methods, parameterized by a small
  adapter spec (program/args/env, `language_id`, initialize params, root detection, feature
  overrides, optional enrichment hooks). `GoplsService` shrinks to that spec. Doing this before
  the second adapter lands is much cheaper than after.

### 3. No file-ownership filter: every changed file is sent to gopls as Go
- **Severity**: medium
- **Where**: `crates/codescope-analysis/src/engine.rs:168–231` (`analyse_file` skips only
  binary/gitlink/unmerged, engine.rs:176; no language/extension/ownership check), feeding
  `crates/codescope-lsp/src/gopls.rs:153–186` (`reopen` sends `didOpen` with hardcoded
  `languageId: "go"` for any path, gopls.rs:171).
- **What**: The pipeline has no notion of which files a language service owns. On a real Go
  branch that also touches `README.md`, `.github/workflows/*.yml`, or `go.sum`, each refresh
  does didClose/didOpen with full text plus a `documentSymbol` request for those files against
  gopls, mislabeled as Go. Failures degrade to per-file notes (non-fatal by design), but the
  requests are wasted, notes accumulate ("worktree symbols unavailable…"), and gopls may emit
  spurious diagnostics for documents it was told are Go.
- **Why it matters**: Language-ownership routing is the *neutral* concept that research 01
  planned ("repo scan assigns each source file a language id → route by owning session") and it
  has no hook anywhere today. For the multi-language future this is the missing seam; for the
  Go-only present it causes avoidable traffic and noise on every mixed change-set.
- **Suggested fix**: Add an ownership predicate to the semantic surface (e.g.
  `LanguageService::handles(&FileId) -> bool`, adapter-defined) and have `analyse_file` skip
  non-owned files with a note. The engine change is language-neutral (it delegates the
  decision), so it does not reintroduce Go knowledge into analysis.

### 4. `SemanticSource`/engine shape assumes exactly one server session per repo
- **Severity**: low (explicit v0 non-goal, but it bounds the neutrality claim)
- **Where**: `crates/codescope-analysis/src/source.rs:29–35` (`fn features(&self) ->
  &FeatureSet` — one capability table per source, not per file),
  `crates/codescope-analysis/src/engine.rs:88–96` (`AnalysisEngine<S>` holds a single `S`),
  `crates/codescope-analysis/src/graph.rs:52` (one global `svc.features()` for the whole build),
  `crates/codescope/src/dispatcher.rs:33` (single `ls_status`).
- **What**: The claim "adding rust-analyzer shouldn't touch analysis" holds for the
  *replacement* scenario (a Rust repo gets rust-analyzer instead of gopls via a new enum
  variant — verified: `source.rs`'s delegation impl and all analysis code compile against the
  enum surface unchanged). It does **not** hold for *coexistence* (Go + Rust in one repo): a
  router implementing `SemanticSource` cannot answer `features()` honestly for two sessions
  with different capability sets, and the engine/dispatcher assume one root and one status.
- **Why it matters**: Multi-language is declared out of scope ("multi-language beyond Go",
  docs/architecture.md non-goals), so this is not a defect — but reviewers of the neutrality
  claim should know exactly where the boundary sits: per-repo single-language neutrality is
  real; per-file multi-language neutrality would require changing `SemanticSource::features`
  (e.g. `features(&self, file: &FileId)`) and the engine.
- **Suggested fix**: None required for v0. When multi-language lands, make `features`
  file-scoped and let a routing `SemanticSource` own N sessions; the rest of graph/changes/digest
  already operates per file/symbol and would survive unchanged.

### 5. `LanguageService::start` documents "detection" that does not exist
- **Severity**: low
- **Where**: `crates/codescope-lsp/src/service.rs:30–33` (doc: "Detection for the prototype: a
  `go.mod` at or above `root` selects gopls"; body: unconditionally
  `Ok(LanguageService::Gopls(GoplsService::start(root).await?))`), with the actual go.mod walk
  in `crates/codescope-lsp/src/gopls.rs:54–56, 653–661` and the failure surfaced as
  `SemanticError::NoRoot` (`crates/codescope-lsp/src/error.rs:88–90`).
- **What**: There is no selection logic; "detection" is just gopls failing to find a `go.mod`.
  On a non-Go repo, `main.rs:60–66` catches the error and runs git-only (top bar then shows
  "gopls: ✗" — see finding 1). Related nit: the 14 method bodies in `service.rs` use the
  irrefutable `let LanguageService::Gopls(s) = self;` (service.rs:38–154), all of which become
  `match` expressions when a second variant appears — mechanical, contained, but worth knowing
  the enum currently compiles only because it has exactly one variant.
- **Why it matters**: `start()` is the designated routing point for language selection; the doc
  overstates what exists, and a reader planning the rust-analyzer addition should not expect a
  detection scaffold to be present.
- **Suggested fix**: Reword the doc ("v0 always starts gopls; non-Go repos fail with `NoRoot`
  and the app degrades to git-only"), or add real marker-file dispatch (`go.mod` → gopls,
  else error) now so the seam is explicit.

### 6. `PushDiagnostics` adapter contract is documented but not implemented
- **Severity**: low
- **Where**: `crates/codescope-lsp/src/capabilities.rs:61–65` ("server adapters that know their
  server pushes (gopls does, quirk 6) mark it `Supported` themselves") and
  `capabilities.rs:115` (`set.set(Feature::PushDiagnostics, Availability::Unknown)`); no
  `set(Feature::PushDiagnostics, …)` call exists anywhere in `gopls.rs` (workspace grep).
- **What**: The gopls adapter never upgrades `PushDiagnostics` to `Supported`, so every
  `FeatureSet` reports it `Unknown`. Nothing currently gates on it (`diagnostics()` reads the
  push cache directly, ungated), so there is no user-visible bug today.
- **Why it matters**: The first adapter is the template the next adapter will be copied from.
  A documented adapter obligation that the reference adapter ignores will propagate; and any
  future consumer that checks `Feature::PushDiagnostics` (e.g. greying out a diagnostics view)
  would wrongly treat gopls as not-supporting.
- **Suggested fix**: Set `Feature::PushDiagnostics → Supported` in `GoplsService::start` after
  `resolve_features`, or delete the claim from `capabilities.rs`.

### 7. Qualified-name convention (`Parent.Child`, `.` separator) is a fixed cross-language choice
- **Severity**: low (style/observation, not a defect)
- **Where**: `crates/codescope-analysis/src/changes.rs:269–279` (`ordered_keys` builds
  `format!("{prefix}.{}", node.name)`), consumed consistently by graph ids
  (`crates/codescope-analysis/src/graph.rs:183–190`), the digest, and AI entity resolution.
- **What**: Nested symbols are qualified with a `.` join of LSP tree names. For gopls this
  yields idiomatic `Greeter.Name` / `(Greeter).Hello`; for rust-analyzer it would yield e.g.
  `impl Widget.render` rather than `Widget::render`. Internally consistent (matching across
  revisions, node ids, and plan validation all use the same string), so correctness is
  unaffected; only display idiom is non-native. Related: research 01's "language-specific
  enrichment hooks (e.g. gopls: method name `(T).M` parsing)" were never built, and there is
  currently no seam for per-adapter name presentation.
- **Suggested fix**: Nothing for v0. If display idiom ever matters, add an optional
  adapter-supplied name-join/format hook; keep the internal identity string as-is.

## Rust-analyzer addition walkthrough (verification of the headline claim)

Changes required, per crate, based on the code as it stands:
- `codescope-lsp`: new adapter module (bulk of the work — see finding 2), `LanguageService`
  variant + `start()` routing, 14 irrefutable-let → `match` conversions (service.rs:38–154).
  Encoding (utf-8 negotiation) and capability parsing already handle rust-analyzer's verified
  behavior, including absent `typeHierarchy` (feature-gated at `graph.rs:148–156` with the
  implementation→subtypes fallback inverting gracefully).
- `codescope-core`: **no change** (SymbolKind/Feature/positions all neutral).
- `codescope-git`: **no change** (no language awareness anywhere; grep-verified).
- `codescope-analysis`: **no change** for single-language repos (enum surface + delegation impl
  unchanged); changes required only for multi-language coexistence (finding 4, explicit
  non-goal).
- `codescope-ai`: **no change** (prompts, schema, validator all neutral; Go strings are test
  data only).
- `codescope-tui`: **one label** (finding 1) — otherwise no change.
- `codescope` (binary): **no change** (`main.rs:60` and `dispatcher.rs:33` are generic).

## Verdict

**Ship** (for the v0 Go-only prototype), with two directives:
1. Fix the `gopls:` label (finding 1) — it is the only place the stated invariant is literally
   false, and the fix is trivial.
2. Treat findings 2 and 3 as mandatory pre-work for the first second-language adapter: extract
   the generic session out of `GoplsService` and add a file-ownership seam before writing
   `RustAnalyzerService`, not after. No redesign is needed — the crate boundaries, the enum
   seam, the trait seam, and the neutral core types are all real and verified.

Severity counts: high 0 · medium 3 · low 4.
