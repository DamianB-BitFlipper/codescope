# 01 — Language-server capabilities & cross-server abstraction

Recovered from sub-agent `research-lsp` verified probes (agent stalled before writing; findings
reconstructed by lead engineer from its session log). All capability claims verified live via raw
LSP initialize handshakes against locally installed servers (Aug 2026).

## Verified capability matrix

| capability            | gopls 0.21 | rust-analyzer 1.96 | clangd 17 (Apple) | pyright | tsls + TS 5.9 |
|-----------------------|-----------|--------------------|-------------------|---------|----------------|
| hover                 | ✅ bool    | ✅ bool             | ✅ bool            | ✅ obj   | ✅ bool         |
| definition            | ✅         | ✅                  | ✅                 | ✅ obj   | ✅              |
| implementation        | ✅         | ✅                  | ✅                 | ❌ null  | ✅              |
| references            | ✅         | ✅                  | ✅                 | ✅ obj   | ✅              |
| documentSymbol        | ✅         | ✅                  | ✅                 | ✅ obj   | ✅              |
| workspaceSymbol       | ✅         | ✅                  | ✅                 | ✅ obj   | ✅              |
| callHierarchy         | ✅         | ✅                  | ✅                 | ✅ bool  | ✅              |
| typeHierarchy         | ✅         | ❌ null             | ✅                 | ❌ null  | ❌ null         |
| positionEncoding resp | absent→utf-16 | **utf-8** (negotiated) | absent→utf-16 | absent→utf-16 | absent→utf-16 |
| serverInfo            | name+VERSION-IS-BUILDINFO-JSON | name+semver | name+string | **null** | **null** |
| textDocumentSync      | obj{change:2,save:{}} | obj | obj{save:true} | **bare int 2** | **bare int 2** |

Key quirks (all verified):
1. **Position encoding**: only rust-analyzer negotiated utf-8. gopls/clangd/pyright/tsls omit
   `positionEncoding` in the response → per LSP 3.17 the default is **utf-16**. The abstraction
   must record the negotiated encoding per server session and convert at the boundary.
2. **Capability shapes differ**: bool, object-with-workDoneProgress (pyright), or absent.
   Treat every capability as `Option<serde_json::Value>`; "supported" = present and not false.
3. **serverInfo is unreliable**: absent (pyright, tsls) or a 2.8KB build-info JSON blob (gopls).
   Never parse it; display raw or "unknown".
4. **textDocumentSync** may be a bare integer (pyright, tsls) or object. Parse both.
5. **tsls requires classic TS 5.x** (tsserver.js); TS 7 native ("tsgo") does not ship it →
   server start succeeds but initialize returns all-null capabilities. Detect all-null caps as
   a broken session and report it.
6. gopls diagnostics are push-only (`textDocument/publishDiagnostics`); no `diagnosticProvider`.
7. gopls `textDocumentSync.change = 2` (incremental); save notification accepted (empty obj).
8. rust-analyzer has NO typeHierarchy; pyright has NO implementation; tsls has NO typeHierarchy.
   The feature-availability model is mandatory, not optional.

## Feature-availability model

```rust
pub enum Feature { DocumentSymbols, WorkspaceSymbols, References, Definition,
                   CallHierarchyIncoming, CallHierarchyOutgoing, TypeHierarchySuper,
                   TypeHierarchySub, Implementation, Hover, PushDiagnostics }
pub struct FeatureSet { map: BTreeMap<Feature, Availability> }
pub enum Availability { Supported, Unsupported, Unknown }
```
Resolved once at initialize from raw capabilities; every query path checks it first and returns
`SemanticError::Unsupported(feature)` instead of sending the request. UI greys out unsupported
views; AI tools report `unsupported` so the model never plans around missing data.

## LanguageService boundary (recommendation)

Enum dispatch, not trait objects (avoids async-trait object-safety pain, keeps one vtable):

```rust
pub enum LanguageService { Gopls(GoplsService), /* future: RustAnalyzer, Clangd, ... */ }
```
Shared plumbing lives in one generic `LspClient` (framing, id matching, cancellation,
position-encoding conversion, capability resolution); each server adapter only supplies:
- spawn command + env
- initialize params (rootUri/workspaceFolders, initializationOptions)
- capability mapping into FeatureSet
- optional language-specific enrichment hooks (e.g. gopls: method name `(T).M` parsing)

Core semantic surface (async, returns `Result<T, SemanticError>` where errors include
`Unsupported`, `Timeout`, `Partial(T)` semantics via evidence structs):
- `document_symbols(file) -> SymbolTree` (hierarchical; degrade flat → top-level only, mark Unknown granularity)
- `workspace_symbols(query) -> Vec<SymbolRef>`
- `references(symbol) -> Evidence<Vec<Location>>`
- `definition/at(point) -> Vec<Location>`
- `incoming_calls(symbol) / outgoing_calls(symbol) -> Evidence<Vec<CallSite>>`
- `implementations(symbol) -> Evidence<Vec<Location>>`
- `type_subtypes/supertypes(symbol) -> Evidence<Vec<SymbolRef>>` (gopls: interfaces)
- `hover(symbol) -> Option<HoverText>`
- `diagnostics(file) -> Vec<Diagnostic>` (from push cache)

`Evidence<T> = { value: T, completeness: Complete|Partial|Unknown, notes: Vec<String> }` —
the honesty layer carried to UI + AI.

## Multi-server / multi-root

- Repo scan assigns each source file a language id → one `LanguageService` instance per language
  per workspace root (gopls: the go.mod dir; tsls: tsconfig dir; fallback: repo root).
- Multiple workspace roots: pass `workspaceFolders` (gopls verified to support it) at init;
  codescope routes requests by file path prefix to the owning session.
- Position encoding: internal model is **byte offsets + line/col in utf-8**; the LspClient
  converts to the session's negotiated encoding (utf-16 for gopls) at the wire boundary only.

## Recommended decisions

1. Internal positions: utf-8 line/char; convert per-session at the wire boundary (utf-16 default).
2. Enum-dispatch `LanguageService` over a generic `LspClient`; per-server adapter ~200 lines.
3. Feature gating at initialize; never send requests the server didn't advertise.
4. Evidence/completeness on all relationship queries — no "complete project graph" claims.
5. Diagnostics: subscribe to publishDiagnostics; cache per file; clear on server restart.
6. Detect all-null-capability initialize responses as broken sessions (tsls-with-TS7 failure mode).
7. gopls init: rootUri = go.mod dir, workspaceFolders set, hierarchicalDocumentSymbolSupport on,
   `general.positionEncodings: ["utf-8","utf-16"]` offered, use whatever comes back (utf-16).
