# Adding another language server

codescope's design goal: a new language server is **one adapter module + one enum variant**.
Nothing in git analysis, change mapping, visualization, or the TUI changes.

## The boundary

`codescope-lsp` exposes a single enum, [`LanguageService`], dispatching to per-server adapters:

```rust
pub enum LanguageService {
    Gopls(GoplsService),
    RustAnalyzer(RustAnalyzerService),
    // Add the next adapter variant here.
}
```

All adapters share one generic [`LspClient`] (Content-Length framing, request-id matching,
push-diagnostics cache, shutdown→exit→kill teardown). An adapter only owns:

1. **Spawn + initialize** — the command, `rootUri`/`workspaceFolders` conventions,
   `initializationOptions`, and the client capabilities to advertise.
2. **Capability resolution** — map the raw `capabilities` object into the shared
   `FeatureSet` (see `capabilities.rs`; it already handles bool / object / bare-int / null).
3. **Optional language enrichment** — e.g. gopls turns `(Greeter).Hello` into a method of
   `Greeter`; keep this server-specific and contained.

Everything above the enum speaks `codescope-core` types (`SymbolTree`, `Evidence<T>`,
`Location`, `SymbolRef`, `FeatureSet`) — a new server never touches them.

## Position encoding

The internal model is **utf-8** (`Position.col` = byte offset). LSP servers default to
**utf-16** unless they negotiate otherwise (only rust-analyzer does, of the servers we probed).
Conversion lives **only** in the adapter at the wire boundary, via `encoding.rs`
(`position_to_wire` / `position_from_wire`), driven by the `positionEncoding` the server
returned at initialize. Do not convert anywhere else.

## Steps

1. Create `crates/codescope-lsp/src/<server>.rs` with a struct holding
   `{ client: LspClient, root, features: FeatureSet, encoding, versions }` — copy `gopls.rs`
   as the template.
2. In `start()`: find the server's project root (using its manifest/workspace markers), spawn
   the server, send `initialize` with
   `hierarchicalDocumentSymbolSupport` + `positionEncodings: ["utf-8","utf-16"]`, resolve
   features (returns `SemanticError::BrokenSession` on all-null capabilities — e.g. tsls with
   TS 7), send `initialized`.
3. Implement the query methods you can support; for the rest, return
   `SemanticError::Unsupported(feature)` (the `require()` helper gates before sending). The
   impact graph silently skips unsupported relations and notes it.
4. Add the enum variant and extend `LanguageService::start` detection for the new marker.
   Current production adapters are gopls and rust-analyzer; one active service is selected, with
   Go winning ties in mixed repositories. Supporting simultaneous services requires a separate
   orchestration change rather than only a new enum variant.
5. Add a live integration test in `tests/` that skips gracefully when the server is absent.

## Verified capability notes (docs/research/01)

| server | current adapter | probed typeHierarchy | positionEncoding | notes |
|---|---|---|---|---|
| gopls 0.21 | ✓ | ✓ (interfaces) | utf-16 (no negotiation) | push-only diagnostics; serverInfo.version is a build-info blob |
| rust-analyzer | ✓ | ✗ | **utf-8** (negotiated) | production Rust adapter |
| clangd 17 | — | ✓ | utf-16 | probe wanted didSave; no adapter yet |
| pyright | — | ✗ | utf-16 | providers are objects; no adapter yet |
| tsls + TS 5.9 | — | ✗ | utf-16 | all-null capabilities with TS 7; no adapter yet |

Treat every capability as `Option<Value>`; "supported" = present and not false/null.
