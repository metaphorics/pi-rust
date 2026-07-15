//! Word navigation — port of packages/tui/src/word-navigation.ts.
//!
//! Cursor positions are **UTF-16 code-unit offsets** (JS string indices),
//! matching TypeScript and `components::input::Input`.

use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use crate::util::{is_punctuation, is_whitespace_char};

/// ASCII punctuation class used *inside* word-like segments
/// (utils.ts `PUNCTUATION_REGEX`). Distinct from `util::is_punctuation`
/// (`^\p{P}+$`), which classifies whole segments.
static PUNCTUATION_IN_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[(){}\[\]<>.,;:'"!?+\-=*/\\|&%^$#@~`]"#).expect("punct in word")
});

#[derive(Debug, Clone)]
struct Segment {
    /// UTF-16 length of this segment.
    len_utf16: usize,
    text: String,
    is_word_like: bool,
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

fn utf16_to_byte(s: &str, utf16_offset: usize) -> usize {
    let mut units = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        if units >= utf16_offset {
            return byte_idx;
        }
        units += ch.len_utf16();
    }
    s.len()
}

/// Word-like ≈ UAX #29 word piece that contains alphanumeric content
/// (Intl.Segmenter `isWordLike`).
fn is_word_like_segment(seg: &str) -> bool {
    // Whole-segment Unicode punctuation is never word-like.
    if is_punctuation(seg) {
        return false;
    }
    seg.chars().any(|c| c.is_alphanumeric())
}

/// Segment `text` into word bounds. When `is_atomic` is provided, any span
/// that the callback accepts as a whole is emitted as a single atomic segment
/// (so paste markers survive UAX#29 splitting).
fn segment_text(text: &str, is_atomic: Option<&dyn Fn(&str) -> bool>) -> Vec<Segment> {
    if let Some(is_atomic) = is_atomic {
        return segment_with_atomic(text, is_atomic);
    }
    segment_word_bounds(text)
}

fn segment_word_bounds(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    for piece in text.split_word_bounds() {
        out.push(Segment {
            len_utf16: utf16_len(piece),
            text: piece.to_owned(),
            is_word_like: is_word_like_segment(piece),
        });
    }
    out
}

fn segment_with_atomic(text: &str, is_atomic: &dyn Fn(&str) -> bool) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut byte = 0usize;
    let bytes = text.as_bytes();

    while byte < bytes.len() {
        if let Some(end) = longest_atomic_from(text, byte, is_atomic) {
            let piece = &text[byte..end];
            out.push(Segment {
                len_utf16: utf16_len(piece),
                text: piece.to_owned(),
                is_word_like: false,
            });
            byte = end;
            continue;
        }

        let rest = &text[byte..];
        let piece = rest.split_word_bounds().next().unwrap_or_else(|| {
            rest.chars()
                .next()
                .map(|c| &rest[..c.len_utf8()])
                .unwrap_or("")
        });
        if piece.is_empty() {
            break;
        }
        out.push(Segment {
            len_utf16: utf16_len(piece),
            text: piece.to_owned(),
            is_word_like: is_word_like_segment(piece),
        });
        byte += piece.len();
    }
    out
}

/// Longest atomic prefix of `text[byte..]` accepted by `is_atomic`, if any.
fn longest_atomic_from(text: &str, byte: usize, is_atomic: &dyn Fn(&str) -> bool) -> Option<usize> {
    let rest = &text[byte..];
    if rest.is_empty() {
        return None;
    }
    let mut last_ok: Option<usize> = None;
    let mut end = 0usize;
    for ch in rest.chars() {
        end += ch.len_utf8();
        let candidate = &rest[..end];
        if is_atomic(candidate) {
            last_ok = Some(byte + end);
        } else if last_ok.is_some() {
            break;
        }
        if end > 128 && last_ok.is_none() {
            break;
        }
    }
    last_ok
}

/// Find cursor after moving one word backward from `cursor` (UTF-16 index).
///
/// Skips trailing whitespace, then stops at the next word/punctuation boundary.
/// `is_atomic_segment` marks paste markers (etc.) that must be skipped as one unit.
pub fn find_word_backward(
    text: &str,
    cursor: usize,
    is_atomic_segment: Option<&dyn Fn(&str) -> bool>,
) -> usize {
    let total = utf16_len(text);
    let cursor = cursor.min(total);
    if cursor == 0 {
        return 0;
    }

    let byte_cursor = utf16_to_byte(text, cursor);
    let text_before = &text[..byte_cursor];
    let mut segments = segment_text(text_before, is_atomic_segment);
    let mut new_cursor = cursor;

    let is_atomic = |s: &str| is_atomic_segment.map(|f| f(s)).unwrap_or(false);

    while let Some(last) = segments.last() {
        if !is_atomic(&last.text) && is_whitespace_char(&last.text) {
            new_cursor = new_cursor.saturating_sub(last.len_utf16);
            segments.pop();
        } else {
            break;
        }
    }

    if segments.is_empty() {
        return new_cursor;
    }

    let last = segments.last().expect("non-empty");

    if is_atomic(&last.text) {
        new_cursor = new_cursor.saturating_sub(last.len_utf16);
    } else if last.is_word_like {
        let segment = &last.text;
        let matches: Vec<_> = PUNCTUATION_IN_WORD.find_iter(segment).collect();
        if matches.is_empty() {
            new_cursor = new_cursor.saturating_sub(last.len_utf16);
        } else {
            let last_match = matches.last().expect("non-empty");
            let keep_bytes = last_match.end();
            let keep_utf16 = utf16_len(&segment[..keep_bytes]);
            new_cursor = new_cursor.saturating_sub(last.len_utf16 - keep_utf16);
        }
    } else {
        // Skip non-word non-whitespace run (TS exclusion).
        // util::is_punctuation already forced pure-P segments non-word-like above.
        while let Some(last) = segments.last() {
            if is_atomic(&last.text) || last.is_word_like || is_whitespace_char(&last.text) {
                break;
            }
            new_cursor = new_cursor.saturating_sub(last.len_utf16);
            segments.pop();
        }
    }

    new_cursor.min(total)
}

/// Find cursor after moving one word forward from `cursor` (UTF-16 index).
///
/// Skips leading whitespace, then stops at the next word/punctuation boundary.
pub fn find_word_forward(
    text: &str,
    cursor: usize,
    is_atomic_segment: Option<&dyn Fn(&str) -> bool>,
) -> usize {
    let total = utf16_len(text);
    let cursor = cursor.min(total);
    if cursor >= total {
        return total;
    }

    let byte_cursor = utf16_to_byte(text, cursor);
    let text_after = &text[byte_cursor..];
    let segments = segment_text(text_after, is_atomic_segment);
    let mut iter = segments.into_iter();
    let mut new_cursor = cursor;

    let is_atomic = |s: &str| is_atomic_segment.map(|f| f(s)).unwrap_or(false);

    let mut next = iter.next();

    while let Some(seg) = next.as_ref() {
        if !is_atomic(&seg.text) && is_whitespace_char(&seg.text) {
            new_cursor += seg.len_utf16;
            next = iter.next();
        } else {
            break;
        }
    }

    let Some(seg) = next else {
        return new_cursor.min(total);
    };

    if is_atomic(&seg.text) {
        new_cursor += seg.len_utf16;
    } else if seg.is_word_like {
        if let Some(m) = PUNCTUATION_IN_WORD.find(&seg.text) {
            let prefix = &seg.text[..m.start()];
            new_cursor += utf16_len(prefix);
        } else {
            new_cursor += seg.len_utf16;
        }
    } else {
        new_cursor += seg.len_utf16;
        for more in iter {
            if is_atomic(&more.text) || more.is_word_like || is_whitespace_char(&more.text) {
                break;
            }
            new_cursor += more.len_utf16;
        }
    }

    new_cursor.min(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backward_over_word() {
        let t = "hello world";
        let end = utf16_len(t);
        let c = find_word_backward(t, end, None);
        assert_eq!(c, 6);
        let c2 = find_word_backward(t, c, None);
        assert_eq!(c2, 0);
    }

    #[test]
    fn forward_over_word() {
        let t = "hello world";
        let c = find_word_forward(t, 0, None);
        assert_eq!(c, 5);
        let c2 = find_word_forward(t, c, None);
        assert_eq!(c2, utf16_len(t));
    }

    #[test]
    fn utf16_emoji_cursor() {
        let t = "hi👍bye";
        assert_eq!(utf16_len(t), 2 + 2 + 3);
        let end = utf16_len(t);
        let c = find_word_backward(t, end, None);
        assert!(c <= end);
        let f = find_word_forward(t, 0, None);
        assert_eq!(f, 2);
    }

    #[test]
    fn atomic_paste_marker() {
        let t = "xx[paste #1 +3 lines]yy";
        let atomic = |s: &str| s.starts_with("[paste #") && s.ends_with(']');
        let end = utf16_len(t);
        let after_yy = find_word_backward(t, end, Some(&atomic));
        assert_eq!(after_yy, utf16_len("xx[paste #1 +3 lines]"));
        let after_marker = find_word_backward(t, after_yy, Some(&atomic));
        assert_eq!(after_marker, utf16_len("xx"));

        let f = find_word_forward(t, 0, Some(&atomic));
        assert_eq!(f, 2);
        let f2 = find_word_forward(t, f, Some(&atomic));
        assert_eq!(f2, utf16_len("xx[paste #1 +3 lines]"));
    }

    #[test]
    fn empty_and_edges() {
        assert_eq!(find_word_backward("", 0, None), 0);
        assert_eq!(find_word_forward("", 0, None), 0);
        assert_eq!(find_word_backward("a", 0, None), 0);
        assert_eq!(find_word_forward("a", 1, None), 1);
    }

    #[test]
    fn unicode_punctuation_run() {
        // U+2026 is Unicode P → util::is_punctuation classifies pure-P segments
        // as non-word-like so the punctuation-run branch can skip them.
        assert!(is_punctuation("…"));
        let t = "hi…bye";
        let end = utf16_len(t);
        // from end: skip "bye" → land at start of "bye" (after ellipsis)
        let c = find_word_backward(t, end, None);
        assert_eq!(c, utf16_len("hi…"));
        // skip ellipsis → land after "hi"
        let c2 = find_word_backward(t, c, None);
        assert_eq!(c2, 2);
        // skip "hi"
        let c3 = find_word_backward(t, c2, None);
        assert_eq!(c3, 0);
        // forward from 0: skip "hi"
        let f = find_word_forward(t, 0, None);
        assert_eq!(f, 2);
        // skip ellipsis
        let f2 = find_word_forward(t, f, None);
        assert_eq!(f2, utf16_len("hi…"));
        // skip "bye"
        let f3 = find_word_forward(t, f2, None);
        assert_eq!(f3, end);
    }
}
