//! Strict LF-only JSONL framing — port of `modes/rpc/jsonl.ts`.
//!
//! Framing is LF-only. Payload strings may contain other Unicode separators
//! such as U+2028 and U+2029; clients must split records on `\n` only.
//! A trailing `\r` is stripped (CRLF tolerance); the final unterminated line
//! is emitted on EOF.

use serde::Serialize;

/// Serialize a single strict JSONL record (JSON + `\n`).
pub fn serialize_json_line<T: Serialize>(value: &T) -> String {
    let mut line = serde_json::to_string(value).expect("JSONL value must serialize");
    line.push('\n');
    line
}

/// Incremental LF-only line decoder (StringDecoder-equivalent: bytes are
/// buffered until a full line exists, so multi-byte UTF-8 sequences never
/// split; invalid UTF-8 decodes to U+FFFD).
#[derive(Debug, Default)]
pub struct JsonlDecoder {
    buffer: Vec<u8>,
}

impl JsonlDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; returns every complete line (without `\n`, trailing `\r`
    /// stripped).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        loop {
            let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') else {
                return lines;
            };
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(String::from_utf8_lossy(&line).into_owned());
        }
    }

    /// Flush the final unterminated line on EOF (oracle `onEnd`).
    pub fn finish(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let mut line = std::mem::take(&mut self.buffer);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(String::from_utf8_lossy(&line).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port of test/rpc-jsonl.test.ts.

    #[test]
    fn serializes_strict_jsonl_without_escaping_unicode_separators() {
        let line = serialize_json_line(&serde_json::json!({ "text": "a\u{2028}b\u{2029}c" }));
        assert!(line.contains("a\u{2028}b\u{2029}c"));
        assert!(line.ends_with('\n'));
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed, serde_json::json!({ "text": "a\u{2028}b\u{2029}c" }));
    }

    #[test]
    fn splits_on_lf_only_and_preserves_separators_inside_payloads() {
        let mut decoder = JsonlDecoder::new();
        let record = serialize_json_line(&serde_json::json!({ "text": "a\u{2028}b\u{2029}c" }));
        let lines = decoder.feed(record.as_bytes());
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed, serde_json::json!({ "text": "a\u{2028}b\u{2029}c" }));
        assert_eq!(decoder.finish(), None);
    }

    #[test]
    fn handles_crlf_delimited_input() {
        let mut decoder = JsonlDecoder::new();
        let lines = decoder.feed(b"{\"a\":1}\r\n{\"b\":2}\r\n");
        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn emits_final_line_without_trailing_lf_on_finish() {
        let mut decoder = JsonlDecoder::new();
        assert!(decoder.feed(b"{\"a\":1}").is_empty());
        assert_eq!(decoder.finish().as_deref(), Some("{\"a\":1}"));
        assert_eq!(decoder.finish(), None);
    }

    #[test]
    fn buffers_split_multibyte_utf8_across_chunks() {
        let mut decoder = JsonlDecoder::new();
        let payload = "{\"t\":\"é\"}\n".as_bytes();
        let (a, b) = payload.split_at(7); // splits the 2-byte é
        assert!(decoder.feed(a).is_empty());
        let lines = decoder.feed(b);
        assert_eq!(lines, vec!["{\"t\":\"é\"}"]);
    }
}
