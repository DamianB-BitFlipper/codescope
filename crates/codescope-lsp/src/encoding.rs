//! Position encoding negotiation and conversion.
//!
//! Internal model: **utf-8** — `Position.character` is a byte offset into the
//! line. LSP 3.17 servers default to **utf-16** code units unless a different
//! encoding is negotiated at initialize time (research 01, quirk 1: only
//! rust-analyzer picked utf-8; gopls omits `positionEncoding` → utf-16).
//!
//! All conversion happens here, at the wire boundary, per session.

use lsp_types::Position;

/// Position encoding negotiated for one server session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PositionEncoding {
    /// `character` counts utf-8 bytes into the line.
    Utf8,
    /// `character` counts utf-16 code units into the line (LSP default).
    #[default]
    Utf16,
}

impl PositionEncoding {
    /// Wire spelling used in `general.positionEncodings`.
    pub fn as_str(self) -> &'static str {
        match self {
            PositionEncoding::Utf8 => "utf-8",
            PositionEncoding::Utf16 => "utf-16",
        }
    }

    /// Resolve the encoding from the server's `capabilities.positionEncoding`
    /// response field. Absent or unrecognized means utf-16 (LSP 3.17 default).
    pub fn from_response_value(value: Option<&serde_json::Value>) -> Self {
        match value.and_then(|v| v.as_str()) {
            Some("utf-8") => PositionEncoding::Utf8,
            Some("utf-32") => {
                // We never offer utf-32; if a server insists, utf-32 code
                // points align with utf-16 for the BMP only — that is wrong
                // for astral chars, so refuse to pretend.
                tracing::warn!(
                    "server selected utf-32 which we never offer; falling back to utf-16"
                );
                PositionEncoding::Utf16
            }
            Some(other) => {
                tracing::warn!(other, "unknown positionEncoding; defaulting to utf-16");
                PositionEncoding::Utf16
            }
            None => PositionEncoding::Utf16,
        }
    }
}

/// Byte offset into `line` corresponding to a utf-16 code-unit column.
/// Columns past the end of the line clamp to `line.len()`; a column that
/// splits a surrogate pair snaps to the end of the astral character.
pub fn utf16_col_to_utf8(line: &str, utf16_col: u32) -> usize {
    let mut units = 0u32;
    for (byte_idx, ch) in line.char_indices() {
        if units >= utf16_col {
            return byte_idx;
        }
        units += ch.len_utf16() as u32;
    }
    line.len()
}

/// utf-16 code-unit column corresponding to a byte offset into `line`.
/// A byte offset past the end clamps to the full line; an offset inside a
/// multi-byte character snaps to the end of that character.
pub fn utf8_col_to_utf16(line: &str, utf8_col: usize) -> u32 {
    let mut units = 0u32;
    for (byte_idx, ch) in line.char_indices() {
        if byte_idx >= utf8_col {
            break;
        }
        units += ch.len_utf16() as u32;
    }
    units
}

/// Internal (utf-8) → wire position, given the text of the line it is on.
pub fn position_to_wire(line: &str, pos: Position, encoding: PositionEncoding) -> Position {
    match encoding {
        PositionEncoding::Utf8 => pos,
        PositionEncoding::Utf16 => Position::new(
            pos.line,
            utf8_col_to_utf16(line, pos.character as usize),
        ),
    }
}

/// Wire → internal (utf-8) position, given the text of the line it is on.
pub fn position_from_wire(line: &str, pos: Position, encoding: PositionEncoding) -> Position {
    match encoding {
        PositionEncoding::Utf8 => pos,
        PositionEncoding::Utf16 => Position::new(
            pos.line,
            utf16_col_to_utf8(line, pos.character) as u32,
        ),
    }
}

/// Extract line `n` (0-based) from `text`, without the line terminator.
/// Returns `None` when the line does not exist.
pub fn line_at(text: &str, n: u32) -> Option<&str> {
    text.split('\n')
        .nth(n as usize)
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 'a'=1B/1u, '😀'=4B/2u, 'b'=1B/1u
    const EMOJI_LINE: &str = "a😀b";
    // 'é' (NFC) = 2B/1u, '中' = 3B/1u, ascii
    const MIXED_LINE: &str = "é中x";

    #[test]
    fn utf8_to_utf16_with_emoji() {
        assert_eq!(utf8_col_to_utf16(EMOJI_LINE, 0), 0);
        assert_eq!(utf8_col_to_utf16(EMOJI_LINE, 1), 1); // after 'a'
        assert_eq!(utf8_col_to_utf16(EMOJI_LINE, 5), 3); // after emoji
        assert_eq!(utf8_col_to_utf16(EMOJI_LINE, 6), 4); // end
    }

    #[test]
    fn utf16_to_utf8_with_emoji() {
        assert_eq!(utf16_col_to_utf8(EMOJI_LINE, 0), 0);
        assert_eq!(utf16_col_to_utf8(EMOJI_LINE, 1), 1);
        assert_eq!(utf16_col_to_utf8(EMOJI_LINE, 3), 5);
        assert_eq!(utf16_col_to_utf8(EMOJI_LINE, 4), 6);
    }

    #[test]
    fn multibyte_bmp_chars() {
        assert_eq!(utf8_col_to_utf16(MIXED_LINE, 2), 1); // after é
        assert_eq!(utf8_col_to_utf16(MIXED_LINE, 5), 2); // after 中
        assert_eq!(utf8_col_to_utf16(MIXED_LINE, 6), 3); // end
        assert_eq!(utf16_col_to_utf8(MIXED_LINE, 1), 2);
        assert_eq!(utf16_col_to_utf8(MIXED_LINE, 2), 5);
        assert_eq!(utf16_col_to_utf8(MIXED_LINE, 3), 6);
    }

    #[test]
    fn roundtrip_all_char_boundaries() {
        let line = "fn x😀é中() { return 1; }";
        let mut boundaries: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
        boundaries.push(line.len());
        for b in boundaries {
            let u16_col = utf8_col_to_utf16(line, b);
            assert_eq!(utf16_col_to_utf8(line, u16_col), b, "byte col {b}");
        }
    }

    #[test]
    fn clamps_past_line_end() {
        assert_eq!(utf16_col_to_utf8(EMOJI_LINE, 999), EMOJI_LINE.len());
        assert_eq!(utf8_col_to_utf16(EMOJI_LINE, 999), 4);
    }

    #[test]
    fn mid_character_columns_snap_forward() {
        // byte 2 is inside the emoji; utf-16 col 2 is inside the surrogate pair
        assert_eq!(utf8_col_to_utf16(EMOJI_LINE, 2), 3);
        assert_eq!(utf16_col_to_utf8(EMOJI_LINE, 2), 5);
    }

    #[test]
    fn empty_line() {
        assert_eq!(utf8_col_to_utf16("", 0), 0);
        assert_eq!(utf16_col_to_utf8("", 0), 0);
        assert_eq!(utf16_col_to_utf8("", 10), 0);
    }

    #[test]
    fn position_conversion_respects_encoding() {
        let pos = Position::new(7, 5);
        assert_eq!(
            position_to_wire(EMOJI_LINE, pos, PositionEncoding::Utf8),
            pos
        );
        assert_eq!(
            position_to_wire(EMOJI_LINE, pos, PositionEncoding::Utf16),
            Position::new(7, 3)
        );
        assert_eq!(
            position_from_wire(EMOJI_LINE, Position::new(7, 3), PositionEncoding::Utf16),
            pos
        );
    }

    #[test]
    fn from_response_value_defaults_to_utf16() {
        assert_eq!(
            PositionEncoding::from_response_value(None),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::from_response_value(Some(&serde_json::Value::Null)),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::from_response_value(Some(&serde_json::json!("utf-8"))),
            PositionEncoding::Utf8
        );
        assert_eq!(
            PositionEncoding::from_response_value(Some(&serde_json::json!("utf-16"))),
            PositionEncoding::Utf16
        );
    }

    #[test]
    fn line_at_handles_crlf_and_bounds() {
        let text = "first\r\nsecond\nthird";
        assert_eq!(line_at(text, 0), Some("first"));
        assert_eq!(line_at(text, 1), Some("second"));
        assert_eq!(line_at(text, 2), Some("third"));
        assert_eq!(line_at(text, 3), None);
    }
}
