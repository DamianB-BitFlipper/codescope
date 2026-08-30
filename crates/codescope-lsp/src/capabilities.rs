//! Server capability resolution into a [`codescope_core::FeatureSet`].
//!
//! Per research 01: capability shapes differ across servers (bool, object with
//! `workDoneProgress`, bare integer for `textDocumentSync`, explicit null).
//! Everything is therefore inspected as raw [`serde_json::Value`] and
//! "supported" means *present and not `false` and not `null`*.
//!
//! The [`Feature`]/[`FeatureSet`]/[`Availability`] types live in
//! `codescope-core` (they flow to the analysis layer, the TUI, and the AI
//! digest); this module only resolves them from the wire.

use codescope_core::{Availability, Feature, FeatureSet};
use lsp_types::TextDocumentSyncKind;
use serde_json::Value;

use crate::error::SemanticError;

/// Gate a query path: anything but [`Availability::Supported`] fails *before*
/// the request is sent (research 01: "never send requests the server didn't
/// advertise").
pub fn require(features: &FeatureSet, feature: Feature) -> Result<(), SemanticError> {
    if features.is_supported(feature) {
        Ok(())
    } else {
        Err(SemanticError::Unsupported(feature))
    }
}

/// Capability keys consulted for the all-null / broken-session check and for
/// feature resolution. Any *present, non-null* one counts as a live server.
const PROVIDER_KEYS: &[&str] = &[
    "textDocumentSync",
    "hoverProvider",
    "definitionProvider",
    "declarationProvider",
    "referencesProvider",
    "documentSymbolProvider",
    "workspaceSymbolProvider",
    "implementationProvider",
    "callHierarchyProvider",
    "typeHierarchyProvider",
    "completionProvider",
    "documentFormattingProvider",
];

/// One capability value: present-and-truthy, present-but-disabled, or absent.
fn capability_state(caps: &Value, key: &str) -> Availability {
    match caps.get(key) {
        None => Availability::Unknown,
        Some(Value::Null) => Availability::Unsupported,
        Some(Value::Bool(false)) => Availability::Unsupported,
        Some(_) => Availability::Supported,
    }
}

/// Resolve a [`FeatureSet`] from the raw `capabilities` object of an
/// initialize result. Returns [`SemanticError::BrokenSession`] when every
/// provider capability is null/absent — the verified "server started but is
/// broken" failure mode (research 01, quirk 5).
///
/// [`Feature::PushDiagnostics`] is left [`Availability::Unknown`]: LSP has no
/// capability key for push diagnostics; server adapters that know their server
/// pushes (gopls does, quirk 6) mark it `Supported` themselves.
pub fn resolve_features(caps: &Value) -> Result<FeatureSet, SemanticError> {
    let caps = match caps {
        Value::Object(_) => caps,
        // `capabilities: null` or missing entirely.
        _ => {
            return Err(SemanticError::BrokenSession(
                "initialize result has no capabilities object".to_string(),
            ))
        }
    };

    let any_provider = PROVIDER_KEYS
        .iter()
        .any(|key| matches!(caps.get(*key), Some(v) if !v.is_null()));
    if !any_provider {
        return Err(SemanticError::BrokenSession(
            "initialize capabilities are null/absent for every provider".to_string(),
        ));
    }

    let mut set = FeatureSet::new();
    set.set(
        Feature::DocumentSymbols,
        capability_state(caps, "documentSymbolProvider"),
    );
    set.set(
        Feature::WorkspaceSymbols,
        capability_state(caps, "workspaceSymbolProvider"),
    );
    set.set(
        Feature::References,
        capability_state(caps, "referencesProvider"),
    );
    set.set(
        Feature::Definition,
        capability_state(caps, "definitionProvider"),
    );
    set.set(
        Feature::Implementation,
        capability_state(caps, "implementationProvider"),
    );
    set.set(Feature::Hover, capability_state(caps, "hoverProvider"));

    let call_hierarchy = capability_state(caps, "callHierarchyProvider");
    set.set(Feature::CallHierarchyIncoming, call_hierarchy);
    set.set(Feature::CallHierarchyOutgoing, call_hierarchy);

    let type_hierarchy = capability_state(caps, "typeHierarchyProvider");
    set.set(Feature::TypeHierarchySuper, type_hierarchy);
    set.set(Feature::TypeHierarchySub, type_hierarchy);

    set.set(Feature::PushDiagnostics, Availability::Unknown);

    Ok(set)
}

/// Parse `textDocumentSync`, which may be a bare integer (pyright, tsls) or an
/// options object (gopls) — research 01, quirk 4.
///
/// Returns `None` when the capability is absent/null or unparseable; an object
/// without a `change` field means documents are not synced
/// ([`TextDocumentSyncKind::NONE`]).
pub fn parse_text_document_sync(caps: &Value) -> Option<TextDocumentSyncKind> {
    fn kind_from_i64(n: i64) -> TextDocumentSyncKind {
        match n {
            1 => TextDocumentSyncKind::FULL,
            2 => TextDocumentSyncKind::INCREMENTAL,
            _ => TextDocumentSyncKind::NONE,
        }
    }
    match caps.get("textDocumentSync") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_i64().map(kind_from_i64),
        Some(obj @ Value::Object(_)) => {
            let change = obj.get("change").and_then(Value::as_i64).map(kind_from_i64);
            // Object without `change` means documents are not synced (0).
            Some(change.unwrap_or(TextDocumentSyncKind::NONE))
        }
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bool_capability_true_is_supported() {
        let set = resolve_features(&json!({"hoverProvider": true, "textDocumentSync": 2}))
            .expect("not broken");
        assert_eq!(set.get(Feature::Hover), Availability::Supported);
    }

    #[test]
    fn bool_capability_false_is_unsupported() {
        let set = resolve_features(&json!({"hoverProvider": false, "textDocumentSync": 2}))
            .expect("not broken");
        assert_eq!(set.get(Feature::Hover), Availability::Unsupported);
    }

    #[test]
    fn object_capability_is_supported() {
        // pyright-style: object with workDoneProgress
        let set = resolve_features(
            &json!({"referencesProvider": {"workDoneProgress": true}, "textDocumentSync": 2}),
        )
        .expect("not broken");
        assert_eq!(set.get(Feature::References), Availability::Supported);
    }

    #[test]
    fn null_capability_is_unsupported() {
        // pyright: implementationProvider is null (research 01 matrix)
        let set = resolve_features(&json!({"implementationProvider": null, "textDocumentSync": 2}))
            .expect("not broken");
        assert_eq!(set.get(Feature::Implementation), Availability::Unsupported);
    }

    #[test]
    fn absent_capability_is_unknown() {
        let set = resolve_features(&json!({"textDocumentSync": 2})).expect("not broken");
        assert_eq!(set.get(Feature::Hover), Availability::Unknown);
        assert!(!set.is_supported(Feature::Hover));
    }

    #[test]
    fn all_null_capabilities_are_a_broken_session() {
        // tsls + TS7 failure mode (research 01, quirk 5)
        let result = resolve_features(&json!({
            "hoverProvider": null,
            "definitionProvider": null,
            "referencesProvider": null,
            "documentSymbolProvider": null,
            "workspaceSymbolProvider": null,
            "implementationProvider": null,
            "callHierarchyProvider": null,
            "typeHierarchyProvider": null,
            "textDocumentSync": null
        }));
        assert!(matches!(result, Err(SemanticError::BrokenSession(_))));
    }

    #[test]
    fn missing_capabilities_object_is_a_broken_session() {
        assert!(matches!(
            resolve_features(&Value::Null),
            Err(SemanticError::BrokenSession(_))
        ));
        assert!(matches!(
            resolve_features(&json!({})),
            Err(SemanticError::BrokenSession(_))
        ));
    }

    #[test]
    fn gopls_like_capabilities_resolve_full_feature_set() {
        // Mirrors the verified gopls 0.21 handshake shape.
        let caps = json!({
            "textDocumentSync": {"change": 2, "openClose": true, "save": {}},
            "hoverProvider": true,
            "definitionProvider": true,
            "referencesProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": true,
            "implementationProvider": true,
            "callHierarchyProvider": true,
            "typeHierarchyProvider": true
        });
        let set = resolve_features(&caps).expect("gopls is not broken");
        for feature in [
            Feature::DocumentSymbols,
            Feature::WorkspaceSymbols,
            Feature::References,
            Feature::Definition,
            Feature::CallHierarchyIncoming,
            Feature::CallHierarchyOutgoing,
            Feature::TypeHierarchySuper,
            Feature::TypeHierarchySub,
            Feature::Implementation,
            Feature::Hover,
        ] {
            assert!(set.is_supported(feature), "{feature:?} should be supported");
        }
        // Push diagnostics are not advertised; adapters mark them explicitly.
        assert_eq!(set.get(Feature::PushDiagnostics), Availability::Unknown);
    }

    #[test]
    fn require_gates_before_sending() {
        let set = resolve_features(&json!({"textDocumentSync": 2})).expect("not broken");
        assert!(matches!(
            require(&set, Feature::References),
            Err(SemanticError::Unsupported(Feature::References))
        ));
        let mut full = FeatureSet::new();
        full.set(Feature::References, Availability::Supported);
        assert!(require(&full, Feature::References).is_ok());
    }

    #[test]
    fn text_document_sync_bare_int() {
        let caps = json!({"textDocumentSync": 2});
        assert_eq!(
            parse_text_document_sync(&caps),
            Some(TextDocumentSyncKind::INCREMENTAL)
        );
        let caps = json!({"textDocumentSync": 1});
        assert_eq!(
            parse_text_document_sync(&caps),
            Some(TextDocumentSyncKind::FULL)
        );
    }

    #[test]
    fn text_document_sync_object() {
        let caps = json!({"textDocumentSync": {"change": 2, "openClose": true, "save": {}}});
        assert_eq!(
            parse_text_document_sync(&caps),
            Some(TextDocumentSyncKind::INCREMENTAL)
        );
        let caps = json!({"textDocumentSync": {"openClose": true}});
        assert_eq!(
            parse_text_document_sync(&caps),
            Some(TextDocumentSyncKind::NONE)
        );
    }

    #[test]
    fn text_document_sync_absent_or_null() {
        assert_eq!(parse_text_document_sync(&json!({})), None);
        assert_eq!(
            parse_text_document_sync(&json!({"textDocumentSync": null})),
            None
        );
    }
}
