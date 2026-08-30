//! LSP base-protocol framing: `Content-Length` headers over a byte stream.
//!
//! The decoder is a streaming state machine: callers feed arbitrary byte
//! chunks and get back zero or more frame events. Malformed frames are
//! reported as [`FrameEvent::Skipped`] and never panic; decoding continues
//! with the bytes after the bad header so a later valid frame still parses
//! (research 08 §3: "client must degrade, never panic").

/// Largest header block we buffer before declaring the stream garbage.
const MAX_HEADER_LEN: usize = 16 * 1024;
/// Largest single message body accepted (defense against a corrupt length).
const MAX_BODY_LEN: usize = 64 * 1024 * 1024;

/// One decoded event from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEvent {
    /// A complete `Content-Length` framed body (not yet JSON-validated).
    Message(Vec<u8>),
    /// Bytes were dropped because they did not form a valid frame header.
    Skipped {
        /// Human-readable reason, already traced by the caller path.
        reason: String,
    },
}

/// Streaming decoder for the LSP base protocol.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed a chunk of bytes; returns every frame event that became complete.
    pub fn feed(&mut self, data: &[u8]) -> Vec<FrameEvent> {
        self.buf.extend_from_slice(data);
        let mut events = Vec::new();
        while let Some(event) = self.try_extract() {
            events.push(event);
        }
        events
    }

    /// Attempt to extract one event from the front of the buffer.
    /// Returns `None` when more bytes are needed.
    fn try_extract(&mut self) -> Option<FrameEvent> {
        let header_end = find_subslice(&self.buf, b"\r\n\r\n");
        let header_end = match header_end {
            Some(pos) => pos,
            None => {
                if self.buf.len() > MAX_HEADER_LEN {
                    // No terminator within a sane header size: drop everything.
                    self.buf.clear();
                    return Some(FrameEvent::Skipped {
                        reason: "header terminator not found within 16 KiB".to_string(),
                    });
                }
                return None;
            }
        };

        let header_bytes = self.buf[..header_end].to_vec();
        let header = String::from_utf8_lossy(&header_bytes);
        let content_length = match parse_content_length(&header) {
            Ok(len) => len,
            Err(reason) => {
                // Skip the bad header block, but resync when a plausible new
                // header start sits inside it: garbage preceding a valid frame
                // must not eat that frame's header. `pos > 0` guarantees
                // progress (a failure at position 0 falls through to draining
                // the whole bad block, so the decoder can never spin).
                let marker = find_subslice(&self.buf[..header_end], b"Content-Length:");
                match marker {
                    Some(pos) if pos > 0 => {
                        self.buf.drain(..pos);
                    }
                    _ => {
                        self.buf.drain(..header_end + 4);
                    }
                }
                return Some(FrameEvent::Skipped { reason });
            }
        };

        if content_length > MAX_BODY_LEN {
            self.buf.drain(..header_end + 4);
            return Some(FrameEvent::Skipped {
                reason: format!("implausible Content-Length {content_length}"),
            });
        }

        let body_start = header_end + 4;
        let body_end = body_start + content_length;
        if self.buf.len() < body_end {
            return None; // wait for the rest of the body
        }
        let body = self.buf[body_start..body_end].to_vec();
        self.buf.drain(..body_end);
        Some(FrameEvent::Message(body))
    }
}

/// Serialize one JSON-RPC message into base-protocol framing.
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    out
}

/// Extract the `Content-Length` value from a decoded header block.
/// Other headers (e.g. `Content-Type`) are ignored; the key match is
/// case-insensitive per the base protocol.
fn parse_content_length(header: &str) -> Result<usize, String> {
    for line in header.split("\r\n") {
        let Some((key, value)) = line.split_once(':') else {
            if !line.trim().is_empty() {
                tracing::debug!(line, "ignoring malformed header line");
            }
            continue;
        };
        if key.trim().eq_ignore_ascii_case("content-length") {
            return value
                .trim()
                .parse::<usize>()
                .map_err(|e| format!("invalid Content-Length value {:?}: {e}", value.trim()));
        }
    }
    Err("missing Content-Length header".to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> Vec<u8> {
        encode_frame(body.as_bytes())
    }

    fn messages(events: Vec<FrameEvent>) -> Vec<Vec<u8>> {
        events
            .into_iter()
            .filter_map(|e| match e {
                FrameEvent::Message(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    fn skips(events: &[FrameEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, FrameEvent::Skipped { .. }))
            .count()
    }

    #[test]
    fn decodes_single_frame() {
        let mut dec = FrameDecoder::new();
        let events = dec.feed(&frame(r#"{"jsonrpc":"2.0","id":1}"#));
        assert_eq!(events.len(), 1);
        assert_eq!(
            messages(events),
            vec![br#"{"jsonrpc":"2.0","id":1}"#.to_vec()]
        );
    }

    #[test]
    fn decodes_frame_split_across_feeds() {
        let mut dec = FrameDecoder::new();
        let data = frame(r#"{"id":1}"#);
        assert!(dec.feed(&data[..7]).is_empty()); // inside header
        assert!(dec.feed(&data[7..20]).is_empty()); // rest of header
        assert!(dec.feed(&data[20..25]).is_empty()); // partial body
        let events = dec.feed(&data[25..]);
        assert_eq!(messages(events), vec![br#"{"id":1}"#.to_vec()]);
    }

    #[test]
    fn decodes_two_frames_in_one_feed() {
        let mut dec = FrameDecoder::new();
        let mut data = frame(r#"{"id":1}"#);
        data.extend_from_slice(&frame(r#"{"id":2}"#));
        let events = dec.feed(&data);
        assert_eq!(
            messages(events),
            vec![br#"{"id":1}"#.to_vec(), br#"{"id":2}"#.to_vec()]
        );
    }

    #[test]
    fn decodes_header_with_extra_fields() {
        let mut dec = FrameDecoder::new();
        let body = br#"{"id":9}"#;
        let raw = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut data = raw.into_bytes();
        data.extend_from_slice(body);
        let events = dec.feed(&data);
        assert_eq!(messages(events), vec![body.to_vec()]);
    }

    #[test]
    fn skips_garbage_then_recovers() {
        let mut dec = FrameDecoder::new();
        let mut data = b"this is not a header\r\n\r\n".to_vec();
        data.extend_from_slice(&frame(r#"{"id":3}"#));
        let events = dec.feed(&data);
        assert_eq!(skips(&events), 1);
        assert_eq!(messages(events), vec![br#"{"id":3}"#.to_vec()]);
    }

    #[test]
    fn skips_header_without_content_length() {
        let mut dec = FrameDecoder::new();
        let mut data = b"Content-Type: text/plain\r\n\r\n".to_vec();
        data.extend_from_slice(&frame(r#"{"id":4}"#));
        let events = dec.feed(&data);
        assert_eq!(skips(&events), 1);
        assert_eq!(messages(events), vec![br#"{"id":4}"#.to_vec()]);
    }

    #[test]
    fn skips_invalid_content_length_value() {
        let mut dec = FrameDecoder::new();
        let mut data = b"Content-Length: banana\r\n\r\n".to_vec();
        data.extend_from_slice(&frame(r#"{"id":5}"#));
        let events = dec.feed(&data);
        assert_eq!(skips(&events), 1);
        assert_eq!(messages(events), vec![br#"{"id":5}"#.to_vec()]);
    }

    #[test]
    fn skips_implausible_content_length() {
        let mut dec = FrameDecoder::new();
        let mut data = format!("Content-Length: {}\r\n\r\n", MAX_BODY_LEN + 1).into_bytes();
        data.extend_from_slice(&frame(r#"{"id":6}"#));
        let events = dec.feed(&data);
        assert_eq!(skips(&events), 1);
        assert_eq!(messages(events), vec![br#"{"id":6}"#.to_vec()]);
    }

    #[test]
    fn oversized_header_is_dropped() {
        let mut dec = FrameDecoder::new();
        let mut data = vec![b'x'; MAX_HEADER_LEN + 1];
        data.extend_from_slice(&frame(r#"{"id":7}"#));
        let events = dec.feed(&data);
        assert_eq!(skips(&events), 1);
        assert_eq!(messages(events), vec![br#"{"id":7}"#.to_vec()]);
    }

    #[test]
    fn empty_input_is_fine() {
        let mut dec = FrameDecoder::new();
        assert!(dec.feed(b"").is_empty());
    }

    #[test]
    fn encode_round_trips_through_decoder() {
        let body = r#"{"jsonrpc":"2.0","method":"exit"}"#;
        let mut dec = FrameDecoder::new();
        let events = dec.feed(&encode_frame(body.as_bytes()));
        assert_eq!(messages(events), vec![body.as_bytes().to_vec()]);
    }
}
