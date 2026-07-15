//! Clipboard helpers — OSC 52 encode + arboard set/get.
//!
//! Ported from packages/coding-agent/src/utils/clipboard.ts (tui has no clipboard.ts).

use std::io::{self, Write};

use base64::{Engine, engine::general_purpose::STANDARD};
use thiserror::Error;

/// Cap on base64-encoded OSC 52 payload length (clipboard.ts).
pub const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard unavailable: {0}")]
    Unavailable(String),
    #[error("failed to copy to clipboard")]
    CopyFailed,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Build an OSC 52 clipboard write sequence (`\x1b]52;c;<b64>\x07`).
///
/// Returns `None` when the base64 payload would exceed [`MAX_OSC52_ENCODED_LENGTH`].
pub fn encode_osc52(text: &str) -> Option<String> {
    let encoded = STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return None;
    }
    Some(format!("\x1b]52;c;{encoded}\x07"))
}

/// Write OSC 52 sequence to `out` (typically stdout). Returns whether emitted.
pub fn write_osc52(out: &mut dyn Write, text: &str) -> io::Result<bool> {
    match encode_osc52(text) {
        Some(seq) => {
            out.write_all(seq.as_bytes())?;
            out.flush()?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Set system clipboard text via arboard.
pub fn set_text(text: &str) -> Result<(), ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Unavailable(e.to_string()))?;
    clipboard
        .set_text(text.to_owned())
        .map_err(|e| ClipboardError::Unavailable(e.to_string()))
}

/// Get system clipboard text via arboard.
pub fn get_text() -> Result<String, ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Unavailable(e.to_string()))?;
    clipboard
        .get_text()
        .map_err(|e| ClipboardError::Unavailable(e.to_string()))
}

/// OSC 52 primary write to `out`; arboard fallback when OSC 52 cannot be emitted
/// (oversized payload or write failure). `force_native` skips OSC 52 and uses arboard only.
pub fn copy_text(
    text: &str,
    out: &mut dyn Write,
    force_native: bool,
) -> Result<(), ClipboardError> {
    if !force_native {
        match write_osc52(out, text) {
            Ok(true) => return Ok(()),
            Ok(false) => {
                // payload too large for OSC 52 — fall through to arboard
            }
            Err(_) => {
                // stdout write failed — try arboard
            }
        }
    }

    set_text(text).map_err(|_| ClipboardError::CopyFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encode_shape() {
        let seq = encode_osc52("hi").expect("fits");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\u{07}'));
        let b64 = &seq["\x1b]52;c;".len()..seq.len() - 1];
        assert_eq!(STANDARD.decode(b64).unwrap(), b"hi");
    }

    #[test]
    fn osc52_rejects_huge() {
        // base64 expands ~4/3; craft text so encoded length exceeds cap
        let huge = "x".repeat(MAX_OSC52_ENCODED_LENGTH);
        assert!(encode_osc52(&huge).is_none());
    }
}
