//! Runtime resource limits shared by language-server adapters.

/// Production language-server resource policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageServiceOptions {
    /// Maximum worker threads requested from the child language server.
    pub max_threads: usize,
}

impl Default for LanguageServiceOptions {
    fn default() -> Self {
        Self { max_threads: 2 }
    }
}

impl LanguageServiceOptions {
    pub(crate) fn normalized(self) -> Self {
        Self {
            max_threads: self.max_threads.max(1),
        }
    }
}
