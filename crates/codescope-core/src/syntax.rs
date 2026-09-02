//! Language-neutral syntax information returned by a language server.
//!
//! Codescope deliberately keeps the LSP token-type spelling instead of reducing it to a
//! closed enum. The standard names (such as `keyword`, `function`, and `string`) work across
//! languages, while server-specific names can flow through unchanged and gain a renderer style
//! later without changing the LSP or analysis interfaces.

use crate::LineRange;

/// One semantic syntax token in Codescope's zero-based, UTF-8 position model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SyntaxToken {
    /// Source range occupied by the token.
    pub range: LineRange,
    /// LSP semantic-token type from the server's negotiated legend.
    pub token_type: String,
    /// Modifier names selected by the token's modifier bitset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

/// Optional syntax tokens for both revisions represented by a unified diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffSyntax {
    /// Tokens for the old/base revision.
    pub old: Vec<SyntaxToken>,
    /// Tokens for the new/worktree revision.
    pub new: Vec<SyntaxToken>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_token_round_trips_standard_and_extension_names() {
        for token_type in ["function", "rustAnalyzer.customToken"] {
            let token = SyntaxToken {
                range: LineRange::new(3, 4, 3, 9),
                token_type: token_type.to_string(),
                modifiers: vec!["declaration".to_string()],
            };
            let json = serde_json::to_string(&token).expect("serialize");
            let decoded: SyntaxToken = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, token);
        }
    }
}
