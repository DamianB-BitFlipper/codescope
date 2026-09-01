//! The gopls adapter: spawn, initialize, overlay management, and translation of gopls
//! responses into `codescope-core` domain types behind the common semantic surface.
//!
//! Verified quirks honored here (docs/research/01-lsp-abstraction.md):
//! - gopls does not negotiate `positionEncoding` → the wire is **utf-16**; all
//!   conversion happens at the boundary via [`crate::encoding`].
//! - Diagnostics are push-only; they come from the client's publish cache.
//! - Hierarchical `DocumentSymbol[]` requires `hierarchicalDocumentSymbolSupport`; a
//!   flat `SymbolInformation[]` response is a degraded, top-level-only fallback
//!   (research 03, verified fact 5).
//! - Go method symbol names carry the receiver: `(Greeter).Hello`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    Diagnostic, Evidence, FeatureSet, FileId, Location, Position, Revision, SymbolKind, SymbolNode,
    SymbolRef, SymbolTree,
};
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::capabilities::{parse_text_document_sync, require, resolve_features};
use crate::client::{LspClient, ShutdownOutcome};
use crate::content_cache::{DocumentSnapshot, OpenDocumentState, SymbolTreeCache};
use crate::detect::go_module_folders;
use crate::encoding::{line_at, position_from_wire, position_to_wire, PositionEncoding};
use crate::error::{LspError, SemanticError};
use crate::options::LanguageServiceOptions;
use crate::uri::{path_from_uri, uri_from_path};

/// Deadline for the very first request (gopls loads the workspace lazily).
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Steady-state request deadline.
const STEADY_TIMEOUT: Duration = Duration::from_secs(10);

fn gopls_command(program: &str, repo_root: &Utf8Path, options: LanguageServiceOptions) -> Command {
    let mut command = Command::new(program);
    command
        .arg("serve")
        .env("GOMAXPROCS", options.max_threads.to_string())
        .current_dir(repo_root.as_std_path());
    command
}

/// gopls session state.
#[derive(Debug)]
pub struct GoplsService {
    client: LspClient,
    /// Absolute repository toplevel (git root). FileIds are relative to this path.
    repo_root: Utf8PathBuf,
    /// Directories containing a `go.mod` or `go.work` that gopls loaded as workspace folders.
    go_roots: Vec<Utf8PathBuf>,
    features: FeatureSet,
    encoding: PositionEncoding,
    /// Open document versions and content identities by absolute path.
    documents: Mutex<HashMap<Utf8PathBuf, OpenDocumentState>>,
    /// Content-addressed symbol trees survive repository epochs without becoming stale.
    symbol_cache: Mutex<SymbolTreeCache>,
    /// Monotonic request counter to distinguish the slow first request.
    request_count: AtomicU64,
}

impl GoplsService {
    /// Spawn gopls rooted at `repo_root` (the git toplevel), loading every `go.mod`
    /// and `go.work` under it as a workspace folder.
    #[tracing::instrument(err)]
    pub async fn start(repo_root: &Utf8Path) -> Result<Self, SemanticError> {
        Self::start_with_options(repo_root, LanguageServiceOptions::default()).await
    }

    /// Spawn gopls with an explicit worker-thread limit.
    #[tracing::instrument(err)]
    pub async fn start_with_options(
        repo_root: &Utf8Path,
        options: LanguageServiceOptions,
    ) -> Result<Self, SemanticError> {
        let options = options.normalized();
        let mut go_roots = go_module_folders(repo_root);
        if go_roots.is_empty() {
            // This should normally be caught by LanguageService::start, but keep the
            // gopls-specific diagnostic for robustness.
            return Err(SemanticError::NoRoot(repo_root.to_path_buf()));
        }
        // If the repo root itself has a go.work, let gopls run in workspace mode with a
        // single root; it will discover the modules listed in the work file.
        if go_roots.iter().any(|r| r == repo_root) {
            go_roots = vec![repo_root.to_path_buf()];
        }

        let program = std::env::var("CODESCOPE_GOPLS").unwrap_or_else(|_| "gopls".to_string());
        let command = gopls_command(&program, repo_root, options);
        let client = LspClient::spawn(command, "gopls")?;

        let root_uri = uri_from_path(repo_root)?;
        let workspace_folders: Vec<Value> = go_roots
            .iter()
            .map(|dir| {
                let uri = uri_from_path(dir)
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| String::new());
                json!({ "uri": uri, "name": dir.file_name().unwrap_or("workspace") })
            })
            .collect();
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri.as_str(),
            "workspaceFolders": workspace_folders,
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
        features.set(
            codescope_core::Feature::PushDiagnostics,
            codescope_core::Availability::Supported,
        );
        let encoding =
            PositionEncoding::from_response_value(init["capabilities"].get("positionEncoding"));
        client.notify("initialized", json!({})).await?;

        tracing::info!(repo_root = %repo_root, ?go_roots, ?encoding, "gopls session initialized");
        Ok(GoplsService {
            client,
            repo_root: repo_root.to_path_buf(),
            go_roots,
            features,
            encoding,
            documents: Mutex::new(HashMap::new()),
            symbol_cache: Mutex::new(SymbolTreeCache::default()),
            request_count: AtomicU64::new(0),
        })
    }

    /// Capabilities resolved at initialize.
    #[must_use]
    pub fn features(&self) -> &FeatureSet {
        &self.features
    }

    /// Absolute repository root that FileIds are relative to.
    #[must_use]
    pub fn repo_root(&self) -> &Utf8Path {
        &self.repo_root
    }

    /// `true` while the server process is alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.client.is_alive()
    }

    /// Absolute path of a repo-relative file id.
    #[must_use]
    pub fn abs_path(&self, file: &FileId) -> Utf8PathBuf {
        self.repo_root.join(file.as_path())
    }

    /// Repo-relative [`FileId`] for an absolute path inside the repo root.
    #[must_use]
    pub fn file_id(&self, abs: &Utf8Path) -> Option<FileId> {
        abs.strip_prefix(&self.repo_root)
            .ok()
            .map(|rel| FileId::new_unchecked(rel.to_path_buf()))
    }

    /// `true` when `abs` is under any of the loaded Go module/workspace folders.
    fn covers(&self, abs: &Utf8Path) -> bool {
        self.go_roots.iter().any(|root| abs.starts_with(root))
    }

    fn timeout(&self) -> Duration {
        if self.request_count.fetch_add(1, Ordering::SeqCst) == 0 {
            FIRST_REQUEST_TIMEOUT
        } else {
            STEADY_TIMEOUT
        }
    }

    /// Read the file once and ensure gopls has that exact content as an open document.
    async fn sync_worktree(&self, file: &FileId) -> Result<DocumentSnapshot, SemanticError> {
        let abs = self.abs_path(file);
        let text = std::fs::read_to_string(&abs).map_err(|source| SemanticError::FileRead {
            path: abs.clone(),
            source,
        })?;
        let snapshot = DocumentSnapshot::new(abs, text);
        self.sync_content(&snapshot.abs, &snapshot.text, snapshot.hash)
            .await?;
        Ok(snapshot)
    }

    /// Close the overlay for `abs` if one is open.
    async fn close(&self, abs: &Utf8Path) -> Result<(), SemanticError> {
        let mut documents = self.documents.lock().await;
        if documents.remove(abs).is_some() {
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

    /// Synchronize a full-text overlay. Unchanged content emits no notification; changed
    /// content advances the LSP version with didChange instead of close/open churn.
    async fn sync_content(
        &self,
        abs: &Utf8Path,
        text: &str,
        hash: u64,
    ) -> Result<bool, SemanticError> {
        let uri = uri_from_path(abs)?;
        let mut documents = self.documents.lock().await;
        if let Some(state) = documents.get_mut(abs) {
            if state.hash == hash {
                return Ok(false);
            }
            state.version = state.version.saturating_add(1);
            state.hash = hash;
            self.client
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {
                            "uri": uri.as_str(),
                            "version": state.version,
                        },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
            return Ok(true);
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
        documents.insert(abs.to_path_buf(), OpenDocumentState { version: 1, hash });
        Ok(true)
    }

    /// Current push-diagnostics for `file`, converted to utf-8 positions.
    #[must_use]
    pub fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        let abs = self.abs_path(file);
        let Ok(uri) = uri_from_path(&abs) else {
            return Vec::new();
        };
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

    fn range_from_wire(&self, text: &str, range: lsp_types::Range) -> codescope_core::LineRange {
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
        let snapshot = self.sync_worktree(file).await?;
        if let Some(tree) =
            self.symbol_cache
                .lock()
                .await
                .get(&snapshot.abs, Revision::Worktree, snapshot.hash)
        {
            return Ok(tree);
        }
        let uri = uri_from_path(&snapshot.abs)?;
        let result = self
            .client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri.as_str() } }),
                self.timeout(),
            )
            .await?;
        let tree = self.symbol_tree(
            file.clone(),
            Revision::Worktree,
            result,
            &snapshot.text,
            &snapshot.abs,
        )?;
        self.symbol_cache.lock().await.insert(
            snapshot.abs,
            Revision::Worktree,
            snapshot.hash,
            tree.clone(),
        );
        Ok(tree)
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
        let base_hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes());
        if let Some(tree) = self
            .symbol_cache
            .lock()
            .await
            .get(&abs, Revision::Base, base_hash)
        {
            return Ok(tree);
        }
        let was_open = self.documents.lock().await.contains_key(&abs);
        let disk = if was_open {
            std::fs::read_to_string(&abs)
                .ok()
                .map(|text| DocumentSnapshot::new(abs.clone(), text))
        } else {
            None
        };
        // Overlay with the base content, query, then restore the worktree view.
        self.sync_content(&abs, content, base_hash).await?;
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
        let restore = match (was_open, &disk) {
            (true, Some(snapshot)) => self
                .sync_content(&abs, &snapshot.text, snapshot.hash)
                .await
                .map(|_| ()),
            _ => self.close(&abs).await,
        };
        let result = result?;
        restore?;
        // Wire positions refer to the overlay `content`, not the disk text.
        let tree = self.symbol_tree(file.clone(), Revision::Base, result, content, &abs)?;
        self.symbol_cache
            .lock()
            .await
            .insert(abs, Revision::Base, base_hash, tree.clone());
        Ok(tree)
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
                    vec![
                        "server returned flat SymbolInformation; nested symbols unavailable"
                            .to_string(),
                    ],
                ))
            }
            Ok(None) => Ok(Evidence::complete(SymbolTree::new(
                file,
                revision,
                Vec::new(),
            ))),
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
        sym.children = sym.children.map(|c| {
            c.into_iter()
                .map(|s| self.doc_symbol_from_wire(text, s))
                .collect()
        });
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
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
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
        let locations: Vec<Location> = match serde_json::from_value::<
            Option<Vec<lsp_types::Location>>,
        >(result)
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
        require(
            &self.features,
            codescope_core::Feature::CallHierarchyIncoming,
        )?;
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
        let refs = match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyIncomingCall>>>(
            result,
        ) {
            Ok(Some(calls)) => calls
                .into_iter()
                .filter_map(|c| self.call_item_to_ref(c.from))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(LspError::Protocol(format!("incomingCalls response: {e}")).into()),
        };
        Ok(Evidence::complete(refs))
    }

    /// Callees of the symbol at `pos`.
    pub async fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(
            &self.features,
            codescope_core::Feature::CallHierarchyOutgoing,
        )?;
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
        let refs = match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyOutgoingCall>>>(
            result,
        ) {
            Ok(Some(calls)) => calls
                .into_iter()
                .filter_map(|c| self.call_item_to_ref(c.to))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(LspError::Protocol(format!("outgoingCalls response: {e}")).into()),
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
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
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
            .request(
                "typeHierarchy/subtypes",
                json!({ "item": item }),
                self.timeout(),
            )
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
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
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
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
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

    /// `true` for Go source files under any loaded Go module/workspace folder.
    #[must_use]
    pub fn handles(&self, file: &FileId) -> bool {
        if file.extension() != Some("go") {
            return false;
        }
        let abs = self.abs_path(file);
        self.covers(&abs)
    }

    /// Graceful teardown.
    pub async fn shutdown(self) {
        let _outcome: ShutdownOutcome = self.client.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gopls_command_limits_go_workers() {
        let command = gopls_command(
            "gopls",
            Utf8Path::new("."),
            LanguageServiceOptions { max_threads: 2 },
        );
        let gomaxprocs = command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == "GOMAXPROCS")
            .and_then(|(_, value)| value)
            .and_then(|value| value.to_str());
        assert_eq!(gomaxprocs, Some("2"));
    }
}
