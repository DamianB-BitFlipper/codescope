//! Semantic-token capability negotiation and wire decoding.

use codescope_core::{LineRange, SyntaxToken};
use lsp_types::{Position, SemanticToken, SemanticTokensResult};
use serde_json::Value;

use crate::encoding::{PositionEncoding, line_at, position_from_wire};
use crate::error::{LspError, SemanticError};

/// The token types and modifiers advertised by a server, in their wire index order.
#[derive(Debug, Clone, Default)]
pub(crate) struct SemanticTokenLegend {
    token_types: Vec<String>,
    token_modifiers: Vec<String>,
}

/// Client capability sent to every adapter. The standard LSP vocabulary is deliberately
/// language-neutral; a newly added server can use the same request without renderer changes.
pub(crate) fn client_capability() -> Value {
    serde_json::json!({
        "dynamicRegistration": false,
        "requests": { "range": false, "full": true },
        "tokenTypes": [
            "namespace", "type", "class", "enum", "interface", "struct",
            "typeParameter", "parameter", "variable", "property", "enumMember",
            "event", "function", "method", "macro", "label", "comment", "string",
            "keyword", "modifier", "number", "regexp", "operator", "decorator"
        ],
        "tokenModifiers": [
            "declaration", "definition", "readonly", "static", "deprecated",
            "abstract", "async", "modification", "documentation", "defaultLibrary"
        ],
        "formats": ["relative"],
        "overlappingTokenSupport": false,
        "multilineTokenSupport": false
    })
}

/// Read a static semantic-token legend from an initialize response.
pub(crate) fn legend_from_capabilities(caps: &Value) -> Option<SemanticTokenLegend> {
    let legend = caps.get("semanticTokensProvider")?.get("legend")?;
    let token_types = legend
        .get("tokenTypes")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    let token_modifiers = match legend.get("tokenModifiers") {
        Some(value) => value
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    (!token_types.is_empty()).then_some(SemanticTokenLegend {
        token_types,
        token_modifiers,
    })
}

/// Decode relative LSP semantic tokens into Codescope's absolute UTF-8 ranges.
pub(crate) fn decode(
    result: Value,
    text: &str,
    encoding: PositionEncoding,
    legend: &SemanticTokenLegend,
) -> Result<Vec<SyntaxToken>, SemanticError> {
    let response = serde_json::from_value::<Option<SemanticTokensResult>>(result)
        .map_err(|error| LspError::Protocol(format!("semanticTokens/full response: {error}")))?;
    let Some(response) = response else {
        return Ok(Vec::new());
    };
    let data = match response {
        SemanticTokensResult::Tokens(tokens) => tokens.data,
        SemanticTokensResult::Partial(tokens) => tokens.data,
    };
    Ok(decode_data(&data, text, encoding, legend))
}

fn decode_data(
    data: &[SemanticToken],
    text: &str,
    encoding: PositionEncoding,
    legend: &SemanticTokenLegend,
) -> Vec<SyntaxToken> {
    let mut line = 0u32;
    let mut start = 0u32;
    let mut decoded = Vec::with_capacity(data.len());
    for token in data {
        line = line.saturating_add(token.delta_line);
        start = if token.delta_line == 0 {
            start.saturating_add(token.delta_start)
        } else {
            token.delta_start
        };
        let Some(source_line) = line_at(text, line) else {
            continue;
        };
        let Some(token_type) = legend.token_types.get(token.token_type as usize) else {
            continue;
        };
        let wire_start = Position::new(line, start);
        let wire_end = Position::new(line, start.saturating_add(token.length));
        let start_utf8 = position_from_wire(source_line, wire_start, encoding);
        let end_utf8 = position_from_wire(source_line, wire_end, encoding);
        if end_utf8.character <= start_utf8.character {
            continue;
        }
        let modifiers = legend
            .token_modifiers
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                u32::try_from(*index)
                    .ok()
                    .and_then(|shift| 1u32.checked_shl(shift))
                    .is_some_and(|mask| token.token_modifiers_bitset & mask != 0)
            })
            .map(|(_, modifier)| modifier.clone())
            .collect();
        decoded.push(SyntaxToken {
            range: LineRange::new(line, start_utf8.character, line, end_utf8.character),
            token_type: token_type.clone(),
            modifiers,
        });
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn legend() -> SemanticTokenLegend {
        legend_from_capabilities(&json!({
            "semanticTokensProvider": {
                "legend": {
                    "tokenTypes": ["keyword", "function"],
                    "tokenModifiers": ["declaration", "async"]
                },
                "full": true
            }
        }))
        .expect("legend")
    }

    #[test]
    fn decodes_relative_positions_and_modifier_bits() {
        let result = json!({ "data": [0, 0, 2, 0, 0, 1, 3, 4, 1, 3] });
        let tokens = decode(
            result,
            "fn main\nlet call",
            PositionEncoding::Utf8,
            &legend(),
        )
        .expect("valid response");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].range, LineRange::new(0, 0, 0, 2));
        assert_eq!(tokens[0].token_type, "keyword");
        assert_eq!(tokens[1].range, LineRange::new(1, 3, 1, 7));
        assert_eq!(tokens[1].modifiers, ["declaration", "async"]);
    }

    #[test]
    fn converts_utf16_columns_to_utf8_without_splitting_unicode() {
        // The function begins after one UTF-16 surrogate pair but four UTF-8 bytes.
        let result = json!({ "data": [0, 2, 4, 1, 0] });
        let tokens =
            decode(result, "😀call", PositionEncoding::Utf16, &legend()).expect("valid response");
        assert_eq!(tokens[0].range, LineRange::new(0, 4, 0, 8));
    }

    #[test]
    fn null_and_unknown_legend_entries_degrade_safely() {
        assert!(
            decode(Value::Null, "", PositionEncoding::Utf8, &legend())
                .expect("null is empty")
                .is_empty()
        );
        let result = json!({ "data": [0, 0, 1, 99, 0] });
        assert!(
            decode(result, "x", PositionEncoding::Utf8, &legend())
                .expect("unknown token type is skipped")
                .is_empty()
        );
    }
}
