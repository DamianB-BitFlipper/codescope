//! In-crate scripted [`SemanticSource`] for unit tests (no server process, never hangs).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use codescope_core::{
    Diagnostic, Evidence, FeatureSet, FileId, Location, Position, SymbolRef, SymbolTree,
};
use codescope_lsp::{LspError, SemanticError};

use crate::source::SemanticSource;

/// One scripted reply for a relationship query.
#[derive(Clone)]
pub(crate) enum Reply {
    Ok(Evidence<Vec<SymbolRef>>),
    Timeout,
}

impl Reply {
    fn to_result(&self, method: &'static str) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        match self {
            Reply::Ok(ev) => Ok(ev.clone()),
            Reply::Timeout => Err(SemanticError::Client(LspError::Timeout {
                method: method.to_string(),
                after: Duration::from_secs(2),
            })),
        }
    }
}

type Key = (FileId, Position);

/// Scripted semantic source: maps `(file, position)` to canned replies per method and
/// records every call for assertions. Unscripted queries return an empty complete result.
#[derive(Default)]
pub(crate) struct ScriptedSource {
    pub features: FeatureSet,
    pub trees: HashMap<FileId, SymbolTree>,
    pub base_trees: HashMap<FileId, SymbolTree>,
    pub diags: HashMap<FileId, Vec<Diagnostic>>,
    pub incoming: HashMap<Key, Reply>,
    pub outgoing: HashMap<Key, Reply>,
    pub impls: HashMap<Key, Reply>,
    pub subtypes: HashMap<Key, Reply>,
    pub calls: Mutex<Vec<String>>,
}

impl ScriptedSource {
    pub(crate) fn record(&self, method: &str, file: &FileId, pos: Position) {
        self.calls
            .lock()
            .expect("calls mutex")
            .push(format!("{method} {file} {}:{}", pos.line, pos.col));
    }

    pub(crate) fn calls_of(&self, method: &str) -> usize {
        self.calls
            .lock()
            .expect("calls mutex")
            .iter()
            .filter(|c| c.starts_with(method))
            .count()
    }

    fn reply(
        &self,
        map: &HashMap<Key, Reply>,
        method: &'static str,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        self.record(method, file, pos);
        match map.get(&(file.clone(), pos)) {
            Some(reply) => reply.to_result(method),
            None => Ok(Evidence::complete(Vec::new())),
        }
    }
}

impl SemanticSource for ScriptedSource {
    fn features(&self) -> &FeatureSet {
        &self.features
    }

    fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        self.diags.get(file).cloned().unwrap_or_default()
    }

    async fn document_symbols(&self, file: &FileId) -> Result<Evidence<SymbolTree>, SemanticError> {
        self.record("document_symbols", file, Position::new(0, 0));
        match self.trees.get(file) {
            Some(tree) => Ok(Evidence::complete(tree.clone())),
            None => Err(SemanticError::NoRoot(file.as_path().to_path_buf())),
        }
    }

    async fn base_document_symbols(
        &self,
        file: &FileId,
        _content: &str,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        self.record("base_document_symbols", file, Position::new(0, 0));
        match self.base_trees.get(file) {
            Some(tree) => Ok(Evidence::complete(tree.clone())),
            None => Err(SemanticError::NoRoot(file.as_path().to_path_buf())),
        }
    }

    async fn references(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<Location>>, SemanticError> {
        self.record("references", file, pos);
        Ok(Evidence::complete(Vec::new()))
    }

    async fn incoming_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        self.reply(&self.incoming, "incoming_calls", file, pos)
    }

    async fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        self.reply(&self.outgoing, "outgoing_calls", file, pos)
    }

    async fn implementations(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        self.reply(&self.impls, "implementations", file, pos)
    }

    async fn type_subtypes(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        self.reply(&self.subtypes, "type_subtypes", file, pos)
    }
}
