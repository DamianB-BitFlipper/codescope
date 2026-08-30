//! Relationships between symbols, the evidence/honesty layer, the feature-availability
//! model, and diagnostics.
//!
//! - [`RelationKind`] labels impact-graph edges and relationship queries.
//! - [`Evidence`] wraps every relationship query result: codescope never claims a complete
//!   project graph (research 01, decision 4).
//! - [`Feature`]/[`FeatureSet`]/[`Availability`] record what the connected language server
//!   advertised at initialize; every query path checks it first (research 01).
//! - [`Diagnostic`] is the push-diagnostics cache entry (gopls is push-only).

use crate::file::FileId;
use crate::position::LineRange;
use std::collections::BTreeMap;

/// Kind of relationship between two symbols (impact-graph edge kinds, research 03/arch.).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// `from` calls `to`.
    Calls,
    /// `from` is called by `to`.
    CalledBy,
    /// `from` implements `to` (e.g. a type implements an interface).
    Implements,
    /// `from` is implemented by `to`.
    ImplementedBy,
    /// `from` references `to`.
    References,
    /// `from` contains `to` (file ⊃ symbol, struct ⊃ field).
    Contains,
    /// `from` is a subtype of `to`.
    SubtypeOf,
    /// `from` is a supertype of `to`.
    SupertypeOf,
}

impl RelationKind {
    /// The inverse relation from the target's perspective, when it exists in this enum.
    ///
    /// `References` and `Contains` have no inverse variant and return `None`.
    #[must_use]
    pub fn inverse(self) -> Option<RelationKind> {
        match self {
            RelationKind::Calls => Some(RelationKind::CalledBy),
            RelationKind::CalledBy => Some(RelationKind::Calls),
            RelationKind::Implements => Some(RelationKind::ImplementedBy),
            RelationKind::ImplementedBy => Some(RelationKind::Implements),
            RelationKind::SubtypeOf => Some(RelationKind::SupertypeOf),
            RelationKind::SupertypeOf => Some(RelationKind::SubtypeOf),
            RelationKind::References | RelationKind::Contains => None,
        }
    }
}

/// How complete an [`Evidence`] value is.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    /// The full answer, as far as the language server knows.
    Complete,
    /// A partial answer (timeout, truncation, degraded server, …).
    Partial,
    /// Completeness could not be determined.
    #[default]
    Unknown,
}

/// A query result plus an honesty layer about its completeness.
///
/// Carried end-to-end to the UI (greying/marking) and the AI digest (research 01).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Evidence<T> {
    /// The result value (possibly partial).
    pub value: T,
    /// How complete `value` is.
    pub completeness: Completeness,
    /// Human-readable caveats (e.g. "timed out after 2s", "server does not index vendor/").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl<T> Evidence<T> {
    /// A complete result with no caveats.
    #[must_use]
    pub fn complete(value: T) -> Self {
        Evidence {
            value,
            completeness: Completeness::Complete,
            notes: Vec::new(),
        }
    }

    /// A partial result; explain why in `notes`.
    #[must_use]
    pub fn partial(value: T, notes: Vec<String>) -> Self {
        Evidence {
            value,
            completeness: Completeness::Partial,
            notes,
        }
    }

    /// A result of unknown completeness.
    #[must_use]
    pub fn unknown(value: T) -> Self {
        Evidence {
            value,
            completeness: Completeness::Unknown,
            notes: Vec::new(),
        }
    }

    /// `true` when the value is known-complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.completeness == Completeness::Complete
    }

    /// Append a caveat note.
    pub fn push_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// Transform the value, keeping completeness and notes.
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Evidence<U> {
        Evidence {
            value: f(self.value),
            completeness: self.completeness,
            notes: self.notes,
        }
    }

    /// Borrow the value as `Evidence<&T>`.
    #[must_use]
    pub fn as_ref(&self) -> Evidence<&T> {
        Evidence {
            value: &self.value,
            completeness: self.completeness,
            notes: self.notes.clone(),
        }
    }
}

/// A semantic capability a language server may or may not support (research 01 matrix).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// `textDocument/documentSymbol` (hierarchical).
    DocumentSymbols,
    /// `workspace/symbol`.
    WorkspaceSymbols,
    /// `textDocument/references`.
    References,
    /// `textDocument/definition`.
    Definition,
    /// `callHierarchy/incomingCalls`.
    CallHierarchyIncoming,
    /// `callHierarchy/outgoingCalls`.
    CallHierarchyOutgoing,
    /// `typeHierarchy/supertypes`.
    TypeHierarchySuper,
    /// `typeHierarchy/subtypes`.
    TypeHierarchySub,
    /// `textDocument/implementation`.
    Implementation,
    /// `textDocument/hover`.
    Hover,
    /// `textDocument/publishDiagnostics` push notifications.
    PushDiagnostics,
}

/// Whether the connected server supports a [`Feature`].
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Advertised at initialize.
    Supported,
    /// Advertised as absent/false, or the server is known to lack it.
    Unsupported,
    /// Not resolved (capability absent in a shape we could not interpret).
    #[default]
    Unknown,
}

/// Capability table resolved once at initialize from raw server capabilities.
///
/// Lookups for features never recorded return [`Availability::Unknown`] — callers treat
/// anything other than [`Availability::Supported`] as "do not send the request".
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeatureSet {
    map: BTreeMap<Feature, Availability>,
}

impl FeatureSet {
    /// An empty set (every feature resolves to [`Availability::Unknown`]).
    #[must_use]
    pub fn new() -> Self {
        FeatureSet {
            map: BTreeMap::new(),
        }
    }

    /// Record the availability of a feature.
    pub fn set(&mut self, feature: Feature, availability: Availability) {
        self.map.insert(feature, availability);
    }

    /// Availability of a feature; [`Availability::Unknown`] when never recorded.
    #[must_use]
    pub fn get(&self, feature: Feature) -> Availability {
        self.map
            .get(&feature)
            .copied()
            .unwrap_or(Availability::Unknown)
    }

    /// `true` only when the feature was recorded as [`Availability::Supported`].
    #[must_use]
    pub fn is_supported(&self, feature: Feature) -> bool {
        self.get(feature) == Availability::Supported
    }

    /// Number of recorded features.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` when nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate over recorded `(feature, availability)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (Feature, Availability)> + '_ {
        self.map.iter().map(|(f, a)| (*f, *a))
    }

    /// Iterate over features recorded as [`Availability::Supported`].
    pub fn supported(&self) -> impl Iterator<Item = Feature> + '_ {
        self.map
            .iter()
            .filter_map(|(f, a)| (*a == Availability::Supported).then_some(*f))
    }
}

/// Diagnostic severity (mirrors LSP).
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Error.
    #[default]
    Error,
    /// Warning.
    Warning,
    /// Information.
    Information,
    /// Hint.
    Hint,
}

impl From<lsp_types::DiagnosticSeverity> for DiagnosticSeverity {
    fn from(sev: lsp_types::DiagnosticSeverity) -> Self {
        use lsp_types::DiagnosticSeverity as L;
        match sev {
            L::ERROR => DiagnosticSeverity::Error,
            L::WARNING => DiagnosticSeverity::Warning,
            L::INFORMATION => DiagnosticSeverity::Information,
            L::HINT => DiagnosticSeverity::Hint,
            _ => DiagnosticSeverity::Error,
        }
    }
}

/// A compiler/linter diagnostic attached to a file (from the push-diagnostics cache).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Repo-relative file path.
    pub file: FileId,
    /// Range the message applies to (UTF-8, zero-based).
    pub range: LineRange,
    /// Severity. LSP allows omitting severity; the LSP layer defaults it to
    /// [`DiagnosticSeverity::Error`].
    pub severity: DiagnosticSeverity,
    /// Diagnostic code, if any (numeric codes stringified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// Source tool, e.g. `go vet` (`diagnostic.source`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Diagnostic {
    /// Convert an LSP diagnostic for `file`.
    ///
    /// The caller must have converted `diag.range` to UTF-8 columns already;
    /// [`LineRange::from_lsp`] is a field rename, not an encoding conversion. A missing
    /// severity defaults to [`DiagnosticSeverity::Error`].
    #[must_use]
    pub fn from_lsp(file: FileId, diag: &lsp_types::Diagnostic) -> Self {
        let severity = diag
            .severity
            .map(DiagnosticSeverity::from)
            .unwrap_or(DiagnosticSeverity::Error);
        let code = diag.code.as_ref().map(|c| match c {
            lsp_types::NumberOrString::Number(n) => n.to_string(),
            lsp_types::NumberOrString::String(s) => s.clone(),
        });
        Diagnostic {
            file,
            range: LineRange::from_lsp(diag.range),
            severity,
            code,
            message: diag.message.clone(),
            source: diag.source.clone(),
        }
    }

    /// `true` for error-severity diagnostics.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == DiagnosticSeverity::Error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_inverse_pairs() {
        assert_eq!(RelationKind::Calls.inverse(), Some(RelationKind::CalledBy));
        assert_eq!(RelationKind::CalledBy.inverse(), Some(RelationKind::Calls));
        assert_eq!(
            RelationKind::Implements.inverse(),
            Some(RelationKind::ImplementedBy)
        );
        assert_eq!(
            RelationKind::ImplementedBy.inverse(),
            Some(RelationKind::Implements)
        );
        assert_eq!(
            RelationKind::SubtypeOf.inverse(),
            Some(RelationKind::SupertypeOf)
        );
        assert_eq!(
            RelationKind::SupertypeOf.inverse(),
            Some(RelationKind::SubtypeOf)
        );
        assert_eq!(RelationKind::References.inverse(), None);
        assert_eq!(RelationKind::Contains.inverse(), None);
    }

    #[test]
    fn evidence_map_preserves_metadata() {
        let ev = Evidence::partial(vec![1, 2], vec!["timed out".to_string()]);
        assert!(!ev.is_complete());
        let doubled = ev.map(|v| v.into_iter().map(|x| x * 2).collect::<Vec<_>>());
        assert_eq!(doubled.value, vec![2, 4]);
        assert_eq!(doubled.completeness, Completeness::Partial);
        assert_eq!(doubled.notes, ["timed out"]);
        let borrowed = doubled.as_ref();
        assert_eq!(borrowed.value, &vec![2, 4]);
    }

    #[test]
    fn feature_set_defaults_to_unknown() {
        let mut fs = FeatureSet::new();
        assert!(fs.is_empty());
        assert_eq!(fs.get(Feature::References), Availability::Unknown);
        assert!(!fs.is_supported(Feature::References));
        fs.set(Feature::References, Availability::Supported);
        fs.set(Feature::TypeHierarchySub, Availability::Unsupported);
        assert!(fs.is_supported(Feature::References));
        assert!(!fs.is_supported(Feature::TypeHierarchySub));
        assert_eq!(fs.len(), 2);
        let supported: Vec<_> = fs.supported().collect();
        assert_eq!(supported, [Feature::References]);
    }

    #[test]
    fn diagnostic_from_lsp_defaults_severity_and_maps_code() {
        let file = FileId::new("main.go").unwrap();
        let mut lsp_diag = lsp_types::Diagnostic::new_simple(
            lsp_types::Range::new(
                lsp_types::Position::new(1, 2),
                lsp_types::Position::new(1, 5),
            ),
            "unused variable".to_string(),
        );
        lsp_diag.code = Some(lsp_types::NumberOrString::Number(42));
        let d = Diagnostic::from_lsp(file, &lsp_diag);
        assert_eq!(d.severity, DiagnosticSeverity::Error); // omitted → Error
        assert_eq!(d.code.as_deref(), Some("42"));
        assert!(d.is_error());
        assert_eq!(d.range, LineRange::new(1, 2, 1, 5));
    }

    #[test]
    fn evidence_serde_roundtrip_generic() {
        let ev = Evidence::complete(vec!["a".to_string()]);
        let json = serde_json::to_string(&ev).unwrap();
        let back: Evidence<Vec<String>> = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}
