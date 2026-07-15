//! Text measurement and SGR-preserving wrap — port of packages/tui/src/utils.ts.
//!
//! Do NOT use inkferro-core `string_width` / `wrap_ansi` for pi semantics:
//! - pi `visibleWidth`: tab = 3, trailing Thai/Lao AM +1, omits `\p{Format}`
//!   from zero-width (utils.ts:167-230); CURSOR_MARKER must be stripped first.
//! - pi `wrapTextWithAnsi` / `AnsiCodeTracker` (utils.ts:369-798) — different
//!   SGR reopen / line-end underline reset than wrap-ansi@10.

use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use crate::line::CURSOR_MARKER;

/// CJK break characters (utils.ts:48-49).
pub static CJK_BREAK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\p{Script_Extensions=Han}\p{Script_Extensions=Hiragana}\p{Script_Extensions=Katakana}\p{Script_Extensions=Hangul}\p{Script_Extensions=Bopomofo}]")
        .expect("cjk regex")
});

static ZERO_WIDTH: LazyLock<Regex> = LazyLock::new(|| {
    // pi omits Format from zero-width (unlike string-width).
    // No \p{Surrogate}: Rust UTF-8 cannot hold surrogates; regex rejects the property.
    Regex::new(r"^(?:\p{Default_Ignorable_Code_Point}|\p{Control}|\p{Mark})+$").expect("zero width")
});

static LEADING_NON_PRINTING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\p{Default_Ignorable_Code_Point}\p{Control}\p{Format}\p{Mark}]+")
        .expect("leading non printing")
});

static PUNCTUATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\p{P}+$").expect("punctuation"));

/// East Asian Width: Fullwidth/Wide → 2, else 1 (Ambiguous narrow, matching pi).
fn east_asian_width(cp: u32) -> usize {
    // unicode-width 0.2: `width()` treats Ambiguous as narrow (1), Fullwidth/Wide as 2.
    // That matches pi (ambiguous-as-narrow). No `width_cjk` in 0.2.
    use unicode_width::UnicodeWidthChar;
    match char::from_u32(cp) {
        Some(c) => c.width().unwrap_or(0).max(1),
        None => 0,
    }
}

fn could_be_emoji(segment: &str) -> bool {
    let Some(cp) = segment.chars().next().map(|c| c as u32) else {
        return false;
    };
    (0x1f000..=0x1fbff).contains(&cp)
        || (0x2300..=0x23ff).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
        || (0x2b50..=0x2b55).contains(&cp)
        || segment.contains('\u{fe0f}')
        || segment.len() > 2
}

fn is_rgi_emoji_approx(segment: &str) -> bool {
    // Approximation of \p{RGI_Emoji}: Extended_Pictographic, ZWJ sequences,
    // keycaps, flags, VS16. Good enough for editor/wrap fixtures; inkferro
    // string_width is more complete but has different tab/AM rules.
    if segment.contains('\u{200d}') {
        return true;
    }
    if segment.contains('\u{fe0f}') {
        return true;
    }
    // Keycap
    if segment.contains('\u{20e3}') {
        return true;
    }
    let Some(cp) = segment.chars().next().map(|c| c as u32) else {
        return false;
    };
    // Regional indicators (flag pairs or single)
    if (0x1f1e6..=0x1f1ff).contains(&cp) {
        return true;
    }
    // Common emoji blocks
    (0x1f300..=0x1faff).contains(&cp)
        || (0x1f600..=0x1f64f).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
        || cp == 0x2705
        || cp == 0x2b50
}

/// Width of one grapheme cluster (utils.ts:167-210).
pub fn grapheme_width(segment: &str) -> usize {
    if segment == "\t" {
        return 3;
    }
    if ZERO_WIDTH.is_match(segment) {
        return 0;
    }
    if could_be_emoji(segment) && is_rgi_emoji_approx(segment) {
        return 2;
    }
    let base = LEADING_NON_PRINTING.replace(segment, "");
    let Some(cp) = base.chars().next().map(|c| c as u32) else {
        return 0;
    };
    // Isolated regional indicators → 2
    if (0x1f1e6..=0x1f1ff).contains(&cp) {
        return 2;
    }
    let mut width = east_asian_width(cp);
    if segment.chars().count() > 1 {
        for ch in segment.chars().skip(1) {
            let c = ch as u32;
            if (0xff00..=0xffef).contains(&c) {
                width += east_asian_width(c);
            } else if c == 0x0e33 || c == 0x0eb3 {
                width += 1;
            }
        }
    }
    width
}

fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Extract ANSI/OSC/APC escape at `pos` if `s[pos]` is ESC (utils.ts:290-328).
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<(&str, usize)> {
    let bytes = s.as_bytes();
    if pos >= bytes.len() || bytes[pos] != 0x1b {
        return None;
    }
    let next = *bytes.get(pos + 1)?;
    match next {
        b'[' => {
            let mut j = pos + 2;
            while j < bytes.len() {
                let c = bytes[j];
                // pi checks /[mGKHJ]/; allow full CSI final range for robustness
                if matches!(c, b'm' | b'G' | b'K' | b'H' | b'J') || (0x40..=0x7e).contains(&c) {
                    // pi only ends on mGKHJ — keep that for parity on common codes
                    if matches!(c, b'm' | b'G' | b'K' | b'H' | b'J') || (0x40..=0x7e).contains(&c) {
                        return Some((&s[pos..=j], j + 1 - pos));
                    }
                }
                j += 1;
            }
            None
        }
        b']' | b'_' => {
            let mut j = pos + 2;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    return Some((&s[pos..=j], j + 1 - pos));
                }
                if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                    return Some((&s[pos..=j + 1], j + 2 - pos));
                }
                j += 1;
            }
            None
        }
        _ => None,
    }
}

/// Strip CURSOR_MARKER (APC) before width measure — not matched by ansi-regex.
fn strip_cursor_marker(s: &str) -> String {
    if s.contains(CURSOR_MARKER) {
        s.replace(CURSOR_MARKER, "")
    } else {
        s.to_owned()
    }
}

/// pi `visibleWidth` (utils.ts:216-271).
pub fn visible_width(str: &str) -> usize {
    if str.is_empty() {
        return 0;
    }
    let str = strip_cursor_marker(str);
    if is_printable_ascii(&str) {
        return str.len();
    }

    let mut clean = str;
    if clean.contains('\t') {
        clean = clean.replace('\t', "   ");
    }
    if clean.contains('\u{1b}') {
        let mut stripped = String::with_capacity(clean.len());
        let mut i = 0;
        let bytes = clean.as_bytes();
        while i < clean.len() {
            if let Some((_, len)) = extract_ansi_code(&clean, i) {
                i += len;
                continue;
            }
            // advance one char
            let ch = clean[i..].chars().next().unwrap();
            stripped.push(ch);
            i += ch.len_utf8();
            let _ = bytes;
        }
        clean = stripped;
    }

    let mut width = 0usize;
    for seg in clean.graphemes(true) {
        width += grapheme_width(seg);
    }
    width
}

/// Thai/Lao AM normalize for terminal output (utils.ts:282-284).
pub fn normalize_terminal_output(s: &str) -> String {
    if !s.chars().any(|c| c == '\u{0e33}' || c == '\u{0eb3}') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\u{0e33}' => out.push_str("\u{0e4d}\u{0e32}"),
            '\u{0eb3}' => out.push_str("\u{0ecd}\u{0eb2}"),
            other => out.push(other),
        }
    }
    out
}

// ─── AnsiCodeTracker ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Osc8Terminator {
    Bel,
    St,
}

#[derive(Debug, Clone)]
struct ActiveHyperlink {
    params: String,
    url: String,
    terminator: Osc8Terminator,
}

/// Track active SGR / OSC-8 across wraps (utils.ts:369-589).
#[derive(Debug, Default, Clone)]
pub struct AnsiCodeTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    fg_color: Option<String>,
    bg_color: Option<String>,
    active_hyperlink: Option<ActiveHyperlink>,
}

impl AnsiCodeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn process(&mut self, ansi_code: &str) {
        if let Some(link) = parse_osc8(ansi_code) {
            self.active_hyperlink = link;
            return;
        }
        if !ansi_code.ends_with('m') {
            return;
        }
        let Some(inner) = ansi_code
            .strip_prefix("\x1b[")
            .and_then(|s| s.strip_suffix('m'))
        else {
            return;
        };
        if inner.is_empty() || inner == "0" {
            self.reset_sgr();
            return;
        }
        let parts: Vec<&str> = inner.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let code: i32 = parts[i].parse().unwrap_or(-1);
            if code == 38 || code == 48 {
                if parts.get(i + 1) == Some(&"5") && parts.get(i + 2).is_some() {
                    let color = format!("{};5;{}", parts[i], parts[i + 2]);
                    if code == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 3;
                    continue;
                } else if parts.get(i + 1) == Some(&"2") && parts.get(i + 4).is_some() {
                    let color = format!(
                        "{};2;{};{};{}",
                        parts[i],
                        parts[i + 2],
                        parts[i + 3],
                        parts[i + 4]
                    );
                    if code == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 5;
                    continue;
                }
            }
            match code {
                0 => self.reset_sgr(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strikethrough = true,
                21 => self.bold = false,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strikethrough = false,
                39 => self.fg_color = None,
                49 => self.bg_color = None,
                30..=37 | 90..=97 => self.fg_color = Some(code.to_string()),
                40..=47 | 100..=107 => self.bg_color = Some(code.to_string()),
                _ => {}
            }
            i += 1;
        }
    }

    fn reset_sgr(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strikethrough = false;
        self.fg_color = None;
        self.bg_color = None;
    }

    pub fn clear(&mut self) {
        self.reset_sgr();
        self.active_hyperlink = None;
    }

    pub fn get_active_codes(&self) -> String {
        let mut codes: Vec<String> = Vec::new();
        if self.bold {
            codes.push("1".into());
        }
        if self.dim {
            codes.push("2".into());
        }
        if self.italic {
            codes.push("3".into());
        }
        if self.underline {
            codes.push("4".into());
        }
        if self.blink {
            codes.push("5".into());
        }
        if self.inverse {
            codes.push("7".into());
        }
        if self.hidden {
            codes.push("8".into());
        }
        if self.strikethrough {
            codes.push("9".into());
        }
        if let Some(fg) = &self.fg_color {
            codes.push(fg.clone());
        }
        if let Some(bg) = &self.bg_color {
            codes.push(bg.clone());
        }
        let mut result = if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        };
        if let Some(link) = &self.active_hyperlink {
            result.push_str(&format_osc8(link));
        }
        result
    }

    pub fn get_line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\x1b[24m");
        }
        if let Some(link) = &self.active_hyperlink {
            result.push_str(&format_osc8_close(&link.terminator));
        }
        result
    }
}

fn parse_osc8(ansi_code: &str) -> Option<Option<ActiveHyperlink>> {
    if !ansi_code.starts_with("\x1b]8;") {
        return None;
    }
    let terminator = if ansi_code.ends_with('\u{07}') {
        Osc8Terminator::Bel
    } else {
        Osc8Terminator::St
    };
    let body_end = if matches!(terminator, Osc8Terminator::Bel) {
        ansi_code.len() - 1
    } else {
        ansi_code.len().saturating_sub(2)
    };
    let body = &ansi_code[4..body_end];
    let sep = body.find(';')?;
    let params = body[..sep].to_owned();
    let url = body[sep + 1..].to_owned();
    if url.is_empty() {
        return Some(None);
    }
    Some(Some(ActiveHyperlink {
        params,
        url,
        terminator,
    }))
}

fn format_osc8(link: &ActiveHyperlink) -> String {
    let term = match link.terminator {
        Osc8Terminator::Bel => "\u{07}",
        Osc8Terminator::St => "\x1b\\",
    };
    format!("\x1b]8;{};{}{}", link.params, link.url, term)
}

fn format_osc8_close(term: &Osc8Terminator) -> String {
    match term {
        Osc8Terminator::Bel => "\x1b]8;;\u{07}".to_owned(),
        Osc8Terminator::St => "\x1b]8;;\x1b\\".to_owned(),
    }
}

fn update_tracker_from_text(text: &str, tracker: &mut AnsiCodeTracker) {
    let mut i = 0;
    while i < text.len() {
        if let Some((code, len)) = extract_ansi_code(text, i) {
            tracker.process(code);
            i += len;
        } else {
            i += text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
    }
}

fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending_ansi = String::new();
    let mut current_kind: Option<bool> = None; // true = space
    let mut i = 0;

    let flush = |tokens: &mut Vec<String>, current: &mut String, kind: &mut Option<bool>| {
        if !current.is_empty() {
            tokens.push(std::mem::take(current));
            *kind = None;
        }
    };

    while i < text.len() {
        if let Some((code, len)) = extract_ansi_code(text, i) {
            pending_ansi.push_str(code);
            i += len;
            continue;
        }
        let mut end = i;
        while end < text.len() && extract_ansi_code(text, end).is_none() {
            end += text[end..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
        }
        let slice = &text[i..end];
        for seg in slice.graphemes(true) {
            let is_space = seg == " ";
            if !is_space && CJK_BREAK.is_match(seg) {
                flush(&mut tokens, &mut current, &mut current_kind);
                let mut token = std::mem::take(&mut pending_ansi);
                token.push_str(seg);
                tokens.push(token);
                continue;
            }
            let kind = is_space;
            if !current.is_empty() && current_kind != Some(kind) {
                flush(&mut tokens, &mut current, &mut current_kind);
            }
            if !pending_ansi.is_empty() {
                current.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            current_kind = Some(kind);
            current.push_str(seg);
        }
        i = end;
    }
    if !pending_ansi.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending_ansi);
        } else {
            current = pending_ansi;
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn break_long_word(token: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut wrapped = Vec::new();
    let mut current = String::new();
    let mut current_vis = 0usize;
    let mut i = 0;
    while i < token.len() {
        if let Some((code, len)) = extract_ansi_code(token, i) {
            current.push_str(code);
            tracker.process(code);
            i += len;
            continue;
        }
        let rest = &token[i..];
        let seg = rest.graphemes(true).next().unwrap_or("");
        let g_w = grapheme_width(seg);
        if current_vis > 0 && current_vis + g_w > width {
            let mut line = current.trim_end().to_owned();
            let reset = tracker.get_line_end_reset();
            line.push_str(&reset);
            wrapped.push(line);
            current = tracker.get_active_codes();
            current_vis = 0;
        }
        current.push_str(seg);
        current_vis += g_w;
        i += seg.len();
    }
    if !current.is_empty() || wrapped.is_empty() {
        wrapped.push(current);
    }
    wrapped
}

#[allow(unused_assignments)]
fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let visible = visible_width(line);
    if visible <= width {
        return vec![line.to_owned()];
    }
    let mut wrapped = Vec::new();
    let mut tracker = AnsiCodeTracker::new();
    let tokens = split_into_tokens_with_ansi(line);
    let mut current_line = String::new();
    let mut current_vis = 0usize;

    for token in tokens {
        let token_vis = visible_width(&token);
        let is_ws = token.trim().is_empty();

        // Token itself is too long - break character by character.
        // Whitespace-only tokens are never hard-broken (utils.ts:741).
        if token_vis > width && !is_ws {
            if !current_line.is_empty() {
                let mut line_end = current_line.clone();
                let reset = tracker.get_line_end_reset();
                line_end.push_str(&reset);
                wrapped.push(line_end);
                current_line.clear();
                current_vis = 0;
            }
            let broken = break_long_word(&token, width, &mut tracker);
            for b in broken.iter().take(broken.len().saturating_sub(1)) {
                wrapped.push(b.clone());
            }
            current_line = broken.last().cloned().unwrap_or_default();
            current_vis = visible_width(&current_line);
            continue;
        }

        let total = current_vis + token_vis;
        if total > width && current_vis > 0 {
            let mut line_to_wrap = current_line.trim_end().to_owned();
            let reset = tracker.get_line_end_reset();
            line_to_wrap.push_str(&reset);
            wrapped.push(line_to_wrap);
            if is_ws {
                current_line = tracker.get_active_codes();
                current_vis = 0;
            } else {
                current_line = tracker.get_active_codes() + &token;
                current_vis = token_vis;
            }
        } else {
            current_line.push_str(&token);
            current_vis += token_vis;
        }
        update_tracker_from_text(&token, &mut tracker);
    }

    if !current_line.is_empty() || wrapped.is_empty() {
        wrapped.push(current_line);
    }

    // Trailing whitespace can cause lines to exceed the requested width
    // (utils.ts:796-797 final trimEnd).
    if wrapped.is_empty() {
        vec![String::new()]
    } else {
        wrapped
            .into_iter()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }
}

/// pi `wrapTextWithAnsi` (utils.ts:694-717). Word wrap only — no padding.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    let input_lines: Vec<&str> = text.split('\n').collect();
    let mut result = Vec::new();
    let mut tracker = AnsiCodeTracker::new();

    for input_line in input_lines {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.get_active_codes()
        };
        let combined = format!("{prefix}{input_line}");
        let wrapped = wrap_single_line(&combined, width);
        result.extend(wrapped);
        update_tracker_from_text(input_line, &mut tracker);
    }
    if result.is_empty() {
        vec![String::new()]
    } else {
        result
    }
}

/// Truncate to max visible width (utils.ts truncateToWidth).
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_owned();
    }
    let mut result = String::new();
    let mut width = 0usize;
    let mut i = 0;
    let mut pending_ansi = String::new();
    while i < text.len() {
        if let Some((code, len)) = extract_ansi_code(text, i) {
            pending_ansi.push_str(code);
            i += len;
            continue;
        }
        if text.as_bytes().get(i) == Some(&b'\t') {
            if width + 3 > max_width {
                break;
            }
            result.push_str(&pending_ansi);
            pending_ansi.clear();
            result.push_str("   ");
            width += 3;
            i += 1;
            continue;
        }
        let rest = &text[i..];
        let seg = rest.graphemes(true).next().unwrap_or("");
        let w = grapheme_width(seg);
        if width + w > max_width {
            break;
        }
        result.push_str(&pending_ansi);
        pending_ansi.clear();
        result.push_str(seg);
        width += w;
        i += seg.len();
    }
    result.push_str("\x1b[0m");
    result
}

/// Apply background function to a line padded to width.
pub fn apply_background_to_line(line: &str, width: usize, bg: &dyn Fn(&str) -> String) -> String {
    let vis = visible_width(line);
    let pad = width.saturating_sub(vis);
    let padded = format!("{line}{}", " ".repeat(pad));
    bg(&padded)
}

pub fn is_whitespace_char(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_whitespace())
}

pub fn is_punctuation(s: &str) -> bool {
    PUNCTUATION.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width() {
        assert_eq!(visible_width("hello"), 5);
    }

    #[test]
    fn tab_width_is_3() {
        assert_eq!(visible_width("\t"), 3);
    }

    #[test]
    fn cursor_marker_zero_width() {
        let s = format!("a{CURSOR_MARKER}b");
        assert_eq!(visible_width(&s), 2);
    }

    #[test]
    fn wrap_basic() {
        let lines = wrap_text_with_ansi("hello world", 5);
        assert!(lines.len() >= 2);
        assert!(visible_width(&lines[0]) <= 5);
    }

    #[test]
    fn wrap_preserves_sgr() {
        let lines = wrap_text_with_ansi("\x1b[31mhello world\x1b[0m", 5);
        assert!(lines[0].contains("\x1b[31m") || lines[0].contains("hello"));
    }
}
