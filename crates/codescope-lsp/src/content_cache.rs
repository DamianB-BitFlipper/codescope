//! Content identities and bounded semantic caches shared by LSP adapters.

use std::collections::{HashMap, VecDeque};

use camino::Utf8PathBuf;
use codescope_core::{Evidence, Revision, SymbolTree};

pub(crate) const SYMBOL_TREE_CACHE_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct DocumentSnapshot {
    pub(crate) abs: Utf8PathBuf,
    pub(crate) text: String,
    pub(crate) hash: u64,
}

impl DocumentSnapshot {
    pub(crate) fn new(abs: Utf8PathBuf, text: String) -> Self {
        let hash = xxhash_rust::xxh3::xxh3_64(text.as_bytes());
        Self { abs, text, hash }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenDocumentState {
    pub(crate) version: i32,
    pub(crate) hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolTreeCacheKey {
    path: Utf8PathBuf,
    revision: Revision,
    hash: u64,
}

/// Small FIFO cache. Repeated keys are refreshed in place; the fixed bound prevents a
/// long review session from retaining every revision ever observed.
#[derive(Debug)]
pub(crate) struct SymbolTreeCache {
    capacity: usize,
    entries: HashMap<SymbolTreeCacheKey, Evidence<SymbolTree>>,
    order: VecDeque<SymbolTreeCacheKey>,
}

impl Default for SymbolTreeCache {
    fn default() -> Self {
        Self::new(SYMBOL_TREE_CACHE_CAPACITY)
    }
}

impl SymbolTreeCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub(crate) fn get(
        &self,
        path: &camino::Utf8Path,
        revision: Revision,
        hash: u64,
    ) -> Option<Evidence<SymbolTree>> {
        self.entries
            .get(&SymbolTreeCacheKey {
                path: path.to_path_buf(),
                revision,
                hash,
            })
            .cloned()
    }

    pub(crate) fn insert(
        &mut self,
        path: Utf8PathBuf,
        revision: Revision,
        hash: u64,
        tree: Evidence<SymbolTree>,
    ) {
        let key = SymbolTreeCacheKey {
            path,
            revision,
            hash,
        };
        if self.entries.insert(key.clone(), tree).is_some() {
            return;
        }
        self.order.push_back(key);
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{FileId, SymbolTree};

    fn tree(path: &str) -> Evidence<SymbolTree> {
        Evidence::complete(SymbolTree::new(
            FileId::new_unchecked(path),
            Revision::Worktree,
            Vec::new(),
        ))
    }

    #[test]
    fn cache_is_content_keyed_and_bounded() {
        let mut cache = SymbolTreeCache::new(2);
        cache.insert("a.go".into(), Revision::Worktree, 1, tree("a.go"));
        cache.insert("b.go".into(), Revision::Worktree, 2, tree("b.go"));
        assert!(cache.get("a.go".into(), Revision::Worktree, 1).is_some());
        assert!(cache.get("a.go".into(), Revision::Worktree, 9).is_none());
        cache.insert("c.go".into(), Revision::Worktree, 3, tree("c.go"));
        assert!(cache.get("a.go".into(), Revision::Worktree, 1).is_none());
        assert_eq!(cache.entries.len(), 2);
    }
}
