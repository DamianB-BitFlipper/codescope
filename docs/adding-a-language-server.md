# Adding another language server

Codescope's design goal: a conventional language server is a small descriptor implementing
`StandardAdapter`. Nothing in overlays, semantic queries, Git analysis, mapping, visualization,
or the TUI is copied.

## The boundary

`StandardAdapter` is the plug-in boundary:

```rust
pub trait StandardAdapter {
    const SERVER_NAME: &'static str;
    const LANGUAGE_ID: &'static str;
    const FILE_EXTENSIONS: &'static [&'static str];

    fn project_root(repo_root: &Utf8Path) -> Result<Utf8PathBuf, SemanticError>;
    fn command(repo_root: &Utf8Path, project_root: &Utf8Path,
               options: LanguageServiceOptions) -> Command;
    fn initialization_options(options: LanguageServiceOptions) -> Value;
}
```

`StandardLspService` supplies initialize, document overlays, capability gating, position
conversion, symbols, references, call and type hierarchy, implementations, hover, semantic
tokens, diagnostics, caches, and graceful shutdown. It uses the server's advertised capabilities
and never calls an unsupported method. A descriptor owns only:

1. server name, LSP language ID, and owned file extension;
2. project-root selection;
3. command construction and resource options;
4. server-specific `initializationOptions`.

Servers requiring nonstandard workspace layout or response enrichment can use a bespoke service;
gopls does this for multi-module workspaces and Go receiver names. Both paths still share
`LspClient` framing, request matching, diagnostics, and teardown.

## Position encoding

The internal model is **utf-8** (`Position.col` = byte offset). LSP servers default to
**utf-16** unless they negotiate otherwise. Conversion lives **only** in the shared session at the
wire boundary, via `encoding.rs`
(`position_to_wire` / `position_from_wire`), driven by the `positionEncoding` the server
returned at initialize. Do not convert anywhere else.

## Steps

1. Add language detection and project-root selection in `detect.rs`.
2. Implement `StandardAdapter` in a small `<server>.rs` descriptor.
3. Register its detection precedence in `LanguageService::start` and its cheap availability probe
   in the binary.
4. Add a live integration test that skips when the server is absent.

Current precedence is Go, Rust, then Python. One active service is selected; simultaneous services
require orchestration beyond the adapter interface.

## Verified capability notes (docs/research/01)

| server | current adapter | probed typeHierarchy | positionEncoding | notes |
|---|---|---|---|---|
| gopls 0.21 | ✓ | ✓ (interfaces) | utf-16 (no negotiation) | push-only diagnostics; serverInfo.version is a build-info blob |
| rust-analyzer | ✓ | ✗ | **utf-8** (negotiated) | production Rust adapter |
| clangd 17 | — | ✓ | utf-16 | probe wanted didSave; no adapter yet |
| pyright | ✓ | ✗ | utf-16 | object-shaped providers; call hierarchy; no semantic tokens |
| tsls + TS 5.9 | — | ✗ | utf-16 | all-null capabilities with TS 7; no adapter yet |

Treat every capability as `Option<Value>`; "supported" = present and not false/null.
