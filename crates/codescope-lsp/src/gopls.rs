//! The gopls adapter: spawn, initialize, overlay management, and translation of gopls
//! responses into `codescope-core` domain types behind the common semantic surface.
//!
//! Verified quirks honored here (docs/research/01-lsp-abstraction.md):
//! - gopls does not negotiate `positionEncoding` → the wire is **utf-16**; all
//!   conversion happens at the boundary via [`crate::encoding`].
//! - Diagnostics are push-only; they come from the client's publish cache.
//! - Hierarchical `DocumentSymbol[]` requires `hierarchicalDocumentSymbolSupport`; a
//!   flat `SymbolInformation[]` response is a degraded, top-level-only fallback
//!   (struct fields are dropped — research 03, verified fact 5).
//! - Go method symbol names carry the receiver: `(Greeter).Hello`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    Diagnostic, Evidence, FeatureSet, FileId, Location, Position, Revision, SymbolKind,
    SymbolNode, SymbolRef, SymbolTree,
};
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::capabilities::{parse_text_document_sync, require, resolve_features};
use crate::client::{LspClient, ShutdownOutcome};
use crate::encoding::{line_at, position_from_wire, position_to_wire, PositionEncoding};
use crate::error::{LspError, SemanticError};
use crate::uri::{path_from_uri, uri_from_path};

/// Deadline for the very first request (gopls loads the workspace lazily).
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Steady-state request deadline.
const STEADY_TIMEOUT: Duration = Duration::from_secs(10);

/// gopls session state.
#[derive(Debug)]
pub struct GoplsService {
    client: LspClient,
    /// Absolute workspace root (dir holding the go.mod we anchored at).
    root: Utf8PathBuf,
    features: FeatureSet,
    encoding: PositionEncoding,
    /// Open document versions by absolute path (for didChange versioning).
    versions: Mutex<HashMap<Utf8PathBuf, i32>>,
    /// Monotonic request counter to distinguish the slow first request.
    request_count: AtomicU64,
}

impl GoplsService {
    /// Spawn gopls rooted at the go.mod directory at or above `root`.
    #[tracing::instrument(err)]
    pub async fn start(root: &Utf8Path) -> Result<Self, SemanticError> {
        let root = find_module_root(root)
            .ok_or_else(|| SemanticError::NoRoot(root.to_path_buf()))?;
        let program = std::env::var("CODESCOPE_GOPLS").unwrap_or_else(|_| "gopls".to_string());

        let mut command = Command::new(&program);
        command.arg("serve").current_dir(root.as_std_path());
        let client = LspClient::spawn(command, "gopls")?;

        let root_uri = uri_from_path(&root)?;
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri.as_str(),
            "workspaceFolders": [{ "uri": root_uri.as_str(), "name": root.file_name().unwrap_or("workspace") }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16"] },
                "textDocument": {
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "callHierarchy": { "dynamicRegistration": false },
                    "typeHierarchy": { "dynamicRegistration": false },
                    "publishDiagnostics": { "relatedInformation": true }
                },
                "workspace": { "workspaceFolders": true, "symbol": {} }
            },
            "initializationOptions": {}
        });
        let init = client
            .request("initialize", params, FIRST_REQUEST_TIMEOUT)
            .await?;
        let _ = parse_text_document_sync(&init["capabilities"]); // gopls: incremental; we close+open instead.
        let mut features = resolve_features(&init["capabilities"])?;
        // gopls pushes diagnostics (research 01 quirk 6); LSP has no capability key for it.
        features.set(codescope_core::Feature::PushDiagnostics, codescope_core::Availability::Supported);
        let encoding = PositionEncoding::from_response_value(init["capabilities"].get("positionEncoding"));
        client.notify("initialized", json!({})).await?;

        tracing::info!(root = %root, ?encoding, "gopls session initialized");
        Ok(GoplsService {
            client,
            root,
            features,
            encoding,
            versions: Mutex::new(HashMap::new()),
            request_count: AtomicU64::new(0),
        })
    }

    /// Capabilities resolved at initialize.
    #[must_use]
    pub fn features(&self) -> &FeatureSet {
        &self.features
    }

    /// Workspace root.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// `true` while the server process is alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.client.is_alive()
    }

    /// Absolute path of a repo-relative file id.
    #[must_use]
    pub fn abs_path(&self, file: &FileId) -> Utf8PathBuf {
        self.root.join(file.as_path())
    }

    /// Repo-relative [`FileId`] for an absolute path inside the root.
    #[must_use]
    pub fn file_id(&self, abs: &Utf8Path) -> Option<FileId> {
        abs.strip_prefix(&self.root)
            .ok()
            .map(|rel| FileId::new_unchecked(rel.to_path_buf()))
    }

    fn timeout(&self) -> Duration {
        if self.request_count.fetch_add(1, Ordering::SeqCst) == 0 {
            FIRST_REQUEST_TIMEOUT
        } else {
            STEADY_TIMEOUT
        }
    }

    /// Ensure gopls has the current disk content of `file` as an open document.
    /// gopls advertises incremental sync only; the simplest correct overlay update is
    /// close + reopen with the full text (research 01: documented choice).
    async fn sync_worktree(&self, file: &FileId) -> Result<Utf8PathBuf, SemanticError> {
        let abs = self.abs_path(file);
        let text = std::fs::read_to_string(&abs).map_err(|source| SemanticError::FileRead {
            path: abs.clone(),
            source,
        })?;
        self.reopen(&abs, &text).await?;
        Ok(abs)
    }

    /// Close the overlay for `abs` if one is open.
    async fn close(&self, abs: &Utf8Path) -> Result<(), SemanticError> {
        let mut versions = self.versions.lock().await;
        if versions.remove(abs).is_some() {
            let uri = uri_from_path(abs)?;
            self.client
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri.as_str() } }),
                )
                .await?;
        }
        Ok(())
    }

    /// Close (if open) then didOpen with `text`.
    async fn reopen(&self, abs: &Utf8Path, text: &str) -> Result<(), SemanticError> {
        let uri = uri_from_path(abs)?;
        let mut versions = self.versions.lock().await;
        if versions.contains_key(abs) {
            self.client
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri.as_str() } }),
                )
                .await?;
            versions.remove(abs);
        }
        self.client
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri.as_str(),
                        "languageId": "go",
                        "version": 1,
                        "text": text,
                    }
                }),
            )
            .await?;
        versions.insert(abs.to_path_buf(), 1);
        Ok(())
    }

    /// Current push-diagnostics for `file`, converted to utf-8 positions.
    #[must_use]
    pub fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        let abs = self.abs_path(file);
        let Ok(uri) = uri_from_path(&abs) else { return Vec::new() };
        let text = std::fs::read_to_string(&abs).unwrap_or_default();
        self.client
            .diagnostics(uri.as_str())
            .iter()
            .map(|d| {
                let mut core = Diagnostic::from_lsp(file.clone(), d);
                core.range = self.range_from_wire(&text, d.range);
                core
            })
            .collect()
    }

    // -- position conversion (wire utf-16 <-> internal utf-8) -------------------

    fn pos_to_wire(&self, text: &str, pos: Position) -> lsp_types::Position {
        let line = line_at(text, pos.line).unwrap_or("");
        position_to_wire(
            line,
            lsp_types::Position::new(pos.line, pos.col),
            self.encoding,
        )
    }

    fn pos_from_wire(&self, text: &str, pos: lsp_types::Position) -> Position {
        let line = line_at(text, pos.line).unwrap_or("");
        let p = position_from_wire(line, pos, self.encoding);
        Position::new(p.line, p.character)
    }

    fn range_from_wire(
        &self,
        text: &str,
        range: lsp_types::Range,
    ) -> codescope_core::LineRange {
        let s = self.pos_from_wire(text, range.start);
        let e = self.pos_from_wire(text, range.end);
        codescope_core::LineRange::new(s.line, s.col, e.line, e.col)
    }

    // -- queries ----------------------------------------------------------------

    /// Hierarchical symbol tree of the worktree content of `file`.
    #[tracing::instrument(err, skip(self))]
    pub async fn document_symbols(
        &self,
        file: &FileId,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        require(&self.features, codescope_core::Feature::DocumentSymbols)?;
        let abs = self.sync_worktree(file).await?;
        let text = std::fs::read_to_string(&abs).unwrap_or_default();
        let uri = uri_from_path(&abs)?;
        let result = self
            .client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri.as_str() } }),
                self.timeout(),
            )
            .await?;
        self.symbol_tree(file.clone(), Revision::Worktree, result, &text, &abs)
    }

    /// Symbol tree of `content` as a temporary overlay (base-revision analysis).
    #[tracing::instrument(err, skip(self, content))]
    pub async fn base_document_symbols(
        &self,
        file: &FileId,
        content: &str,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        require(&self.features, codescope_core::Feature::DocumentSymbols)?;
        let abs = self.abs_path(file);
        let disk = std::fs::read_to_string(&abs).ok();
        // Overlay with the base content, query, then restore the worktree view.
        self.reopen(&abs, content).await?;
        let uri = uri_from_path(&abs)?;
        let result = self
            .client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri.as_str() } }),
                self.timeout(),
            )
            .await;
        // Restore the worktree view. For a deleted file, close the overlay instead of
        // reopening an empty document (F2: an empty overlay produces phantom diagnostics).
        let restore = match &disk {
            Some(text) => self.reopen(&abs, text).await,
            None => self.close(&abs).await,
        };
        let result = result?;
        restore?;
        // Wire positions refer to the overlay `content`, not the disk text.
        self.symbol_tree(file.clone(), Revision::Base, result, content, &abs)
    }

    fn symbol_tree(
        &self,
        file: FileId,
        revision: Revision,
        result: Value,
        text: &str,
        abs: &Utf8Path,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        match serde_json::from_value::<Option<lsp_types::DocumentSymbolResponse>>(result) {
            Ok(Some(lsp_types::DocumentSymbolResponse::Nested(symbols))) => {
                let symbols = symbols
                    .into_iter()
                    .map(|s| self.doc_symbol_from_wire(text, s))
                    .collect();
                let mut tree = SymbolTree::from_document_symbols(file, revision, symbols);
                tree.sort_recursive();
                Ok(Evidence::complete(tree))
            }
            Ok(Some(lsp_types::DocumentSymbolResponse::Flat(infos))) => {
                // Degraded fallback (research 03 fact 5): flat SymbolInformation drops
                // struct fields. Build a top-level-only tree and mark it partial.
                let mut roots: Vec<SymbolNode> = Vec::new();
                for (i, info) in infos.into_iter().enumerate() {
                    if path_from_uri(&info.location.uri).ok().as_ref() != Some(&abs.to_path_buf()) {
                        continue;
                    }
                    let range = self.range_from_wire(text, info.location.range);
                    roots.push(SymbolNode {
                        id: codescope_core::SymbolId::new(i.to_string()),
                        name: info.name,
                        detail: None,
                        kind: SymbolKind::from(info.kind),
                        range,
                        selection: range,
                        children: Vec::new(),
                    });
                }
                let mut tree = SymbolTree::new(file, revision, roots);
                tree.sort_recursive();
                Ok(Evidence::partial(
                    tree,
                    vec!["server returned flat SymbolInformation; nested symbols unavailable"
                        .to_string()],
                ))
            }
            Ok(None) => Ok(Evidence::complete(SymbolTree::new(file, revision, Vec::new()))),
            Err(e) => Err(LspError::Protocol(format!("documentSymbol response: {e}")).into()),
        }
    }

    fn doc_symbol_from_wire(
        &self,
        text: &str,
        sym: lsp_types::DocumentSymbol,
    ) -> lsp_types::DocumentSymbol {
        // Convert position encoding recursively; core's from_document_symbols only renames fields.
        let mut sym = sym;
        sym.range = self.range_lsp_from_wire(text, sym.range);
        sym.selection_range = self.range_lsp_from_wire(text, sym.selection_range);
        sym.children = sym
            .children
            .map(|c| c.into_iter().map(|s| self.doc_symbol_from_wire(text, s)).collect());
        sym
    }

    fn range_lsp_from_wire(&self, text: &str, r: lsp_types::Range) -> lsp_types::Range {
        lsp_types::Range::new(
            {
                let p = self.pos_from_wire(text, r.start);
                lsp_types::Position::new(p.line, p.col)
            },
            {
                let p = self.pos_from_wire(text, r.end);
                lsp_types::Position::new(p.line, p.col)
            },
        )
    }

    /// Reference sites of the symbol at `pos`.
    #[tracing::instrument(err, skip(self))]
    pub async fn references(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<Location>>, SemanticError> {
        require(&self.features, codescope_core::Feature::References)?;
        let abs = self.sync_worktree(file).await?;
        let text = std::fs::read_to_string(&abs).unwrap_or_default();
        let uri = uri_from_path(&abs)?;
        let wire = self.pos_to_wire(&text, pos);
        let result = self
            .client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character },
                    "context": { "includeDeclaration": true }
                }),
                self.timeout(),
            )
            .await?;
        let locations: Vec<Location> = match serde_json::from_value::<Option<Vec<lsp_types::Location>>>(result)
        {
            Ok(Some(locs)) => locs
                .into_iter()
                .filter_map(|l| self.location_from_wire(l))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(LspError::Protocol(format!("references response: {e}")).into()),
        };
        Ok(Evidence::complete(locations))
    }

    /// Callers of the symbol at `pos`.
    pub async fn incoming_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(&self.features, codescope_core::Feature::CallHierarchyIncoming)?;
        let item = match self.prepare_call_hierarchy(file, pos).await? {
            Some(i) => i,
            None => return Ok(Evidence::complete(Vec::new())),
        };
        let result = self
            .client
            .request(
                "callHierarchy/incomingCalls",
                json!({ "item": item }),
                self.timeout(),
            )
            .await?;
        let refs = match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyIncomingCall>>>(result)
        {
            Ok(Some(calls)) => calls
                .into_iter()
                .filter_map(|c| self.call_item_to_ref(c.from))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => {
                return Err(LspError::Protocol(format!("incomingCalls response: {e}")).into())
            }
        };
        Ok(Evidence::complete(refs))
    }

    /// Callees of the symbol at `pos`.
    pub async fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(&self.features, codescope_core::Feature::CallHierarchyOutgoing)?;
        let item = match self.prepare_call_hierarchy(file, pos).await? {
            Some(i) => i,
            None => return Ok(Evidence::complete(Vec::new())),
        };
        let result = self
            .client
            .request(
                "callHierarchy/outgoingCalls",
                json!({ "item": item }),
                self.timeout(),
            )
            .await?;
        let refs = match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyOutgoingCall>>>(result)
        {
            Ok(Some(calls)) => calls
                .into_iter()
                .filter_map(|c| self.call_item_to_ref(c.to))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => {
                return Err(LspError::Protocol(format!("outgoingCalls response: {e}")).into())
            }
        };
        Ok(Evidence::complete(refs))
    }

    /// Implementations of the interface-ish symbol at `pos`.
    pub async fn implementations(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(&self.features, codescope_core::Feature::Implementation)?;
        let abs = self.sync_worktree(file).await?;
        let text = std::fs::read_to_string(&abs).unwrap_or_default();
        let uri = uri_from_path(&abs)?;
        let wire = self.pos_to_wire(&text, pos);
        let result = self
            .client
            .request(
                "textDocument/implementation",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character }
                }),
                self.timeout(),
            )
            .await?;
        let refs = self.goto_response_to_refs(result, "implementation")?;
        Ok(Evidence::complete(refs))
    }

    /// Subtypes of the type symbol at `pos` (for a Go interface: its implementers).
    pub async fn type_subtypes(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(&self.features, codescope_core::Feature::TypeHierarchySub)?;
        let item = match self.prepare_type_hierarchy(file, pos).await? {
            Some(i) => i,
            None => return Ok(Evidence::complete(Vec::new())),
        };
        let result = self
            .client
            .request("typeHierarchy/subtypes", json!({ "item": item }), self.timeout())
            .await?;
        let refs = match serde_json::from_value::<Option<Vec<lsp_types::TypeHierarchyItem>>>(result)
        {
            Ok(Some(items)) => items
                .into_iter()
                .filter_map(|i| self.type_item_to_ref(i))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(LspError::Protocol(format!("subtypes response: {e}")).into()),
        };
        Ok(Evidence::complete(refs))
    }

    // -- prepare helpers ---------------------------------------------------------

    async fn prepare_call_hierarchy(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Option<lsp_types::CallHierarchyItem>, SemanticError> {
        let abs = self.sync_worktree(file).await?;
        let text = std::fs::read_to_string(&abs).unwrap_or_default();
        let uri = uri_from_path(&abs)?;
        let wire = self.pos_to_wire(&text, pos);
        let result = self
            .client
            .request(
                "textDocument/prepareCallHierarchy",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character }
                }),
                self.timeout(),
            )
            .await?;
        match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyItem>>>(result) {
            Ok(Some(items)) => Ok(items.into_iter().next()),
            Ok(None) => Ok(None),
            Err(e) => Err(LspError::Protocol(format!("prepareCallHierarchy: {e}")).into()),
        }
    }

    async fn prepare_type_hierarchy(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Option<lsp_types::TypeHierarchyItem>, SemanticError> {
        let abs = self.sync_worktree(file).await?;
        let text = std::fs::read_to_string(&abs).unwrap_or_default();
        let uri = uri_from_path(&abs)?;
        let wire = self.pos_to_wire(&text, pos);
        let result = self
            .client
            .request(
                "textDocument/prepareTypeHierarchy",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character }
                }),
                self.timeout(),
            )
            .await?;
        match serde_json::from_value::<Option<Vec<lsp_types::TypeHierarchyItem>>>(result) {
            Ok(Some(items)) => Ok(items.into_iter().next()),
            Ok(None) => Ok(None),
            Err(e) => Err(LspError::Protocol(format!("prepareTypeHierarchy: {e}")).into()),
        }
    }

    // -- wire → domain conversion ------------------------------------------------

    fn location_from_wire(&self, loc: lsp_types::Location) -> Option<Location> {
        let abs = path_from_uri(&loc.uri).ok()?;
        let file = self.file_id(&abs)?;
        let text = std::fs::read_to_string(&abs).unwrap_or_default();
        Some(Location {
            file,
            range: self.range_from_wire(&text, loc.range),
        })
    }

    fn call_item_to_ref(&self, item: lsp_types::CallHierarchyItem) -> Option<SymbolRef> {
        let abs = path_from_uri(&item.uri).ok()?;
        let file = self.file_id(&abs)?;
        Some(SymbolRef {
            file,
            name: item.name,
            kind: SymbolKind::from(item.kind),
        })
    }

    fn type_item_to_ref(&self, item: lsp_types::TypeHierarchyItem) -> Option<SymbolRef> {
        let abs = path_from_uri(&item.uri).ok()?;
        let file = self.file_id(&abs)?;
        Some(SymbolRef {
            file,
            name: item.name,
            kind: SymbolKind::from(item.kind),
        })
    }

    fn goto_response_to_refs(
        &self,
        result: Value,
        what: &str,
    ) -> Result<Vec<SymbolRef>, SemanticError> {
        let parsed: Option<lsp_types::GotoDefinitionResponse> = serde_json::from_value(result)
            .map_err(|e| LspError::Protocol(format!("{what} response: {e}")))?;
        let mut out = Vec::new();
        let mut push_loc = |loc: lsp_types::Location| {
            if let Some(location) = self.location_from_wire(loc) {
                out.push(SymbolRef {
                    file: location.file,
                    // Implementation responses carry locations, not names; use the
                    // range-derived placeholder name (enrichment is a later step).
                    name: format!("{}:{}", location.range.start_line, location.range.start_col),
                    kind: SymbolKind::Unknown,
                });
            }
        };
        match parsed {
            Some(lsp_types::GotoDefinitionResponse::Scalar(l)) => push_loc(l),
            Some(lsp_types::GotoDefinitionResponse::Array(ls)) => {
                for l in ls {
                    push_loc(l);
                }
            }
            Some(lsp_types::GotoDefinitionResponse::Link(ls)) => {
                for l in ls {
                    let abs = match path_from_uri(&l.target_uri) {
                        Ok(a) => a,
                        Err(_) => continue,
                    };
                    if let Some(file) = self.file_id(&abs) {
                        let text = std::fs::read_to_string(&abs).unwrap_or_default();
                        let range = self.range_from_wire(&text, l.target_selection_range);
                        out.push(SymbolRef {
                            file,
                            name: format!("{}:{}", range.start_line, range.start_col),
                            kind: SymbolKind::Unknown,
                        });
                    }
                }
            }
            None => {}
        }
        Ok(out)
    }

    /// `true` for Go source files (the only files gopls should see).
    #[must_use]
    pub fn handles(&self, file: &FileId) -> bool {
        file.extension() == Some("go")
    }

    /// Graceful teardown.
    pub async fn shutdown(self) {
        let _outcome: ShutdownOutcome = self.client.shutdown().await;
    }
}

/// Walk up from `start` to the nearest directory containing a `go.mod`.
fn find_module_root(start: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("go.mod").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}
