//! Status enums rendered by the TUI status bar.

/// Lifecycle of the language-server subsystem.
///
/// `Degraded` covers sessions that answer but with reduced quality (e.g. flat
/// `SymbolInformation` fallback, or an all-null-capability initialize that kept running —
/// research 01 quirk 5). `Failed` means the session is unusable; semantic views fall back
/// to git-only data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LsStatus {
    /// Process spawning / initialize handshake in progress.
    Starting,
    /// Initialized; workspace indexing in progress.
    Indexing,
    /// Fully operational.
    Ready,
    /// Answering with reduced quality (flat symbols, missing features, broken-session
    /// heuristics fired).
    Degraded,
    /// Unusable (spawn failed, initialize failed, or crashed without restart).
    Failed,
}

impl LsStatus {
    /// `true` when semantic queries are worth sending (`Ready` or `Degraded`).
    #[must_use]
    pub fn is_usable(&self) -> bool {
        matches!(self, LsStatus::Ready | LsStatus::Degraded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usability() {
        assert!(LsStatus::Ready.is_usable());
        assert!(LsStatus::Degraded.is_usable());
        assert!(!LsStatus::Starting.is_usable());
        assert!(!LsStatus::Indexing.is_usable());
        assert!(!LsStatus::Failed.is_usable());
    }
}
