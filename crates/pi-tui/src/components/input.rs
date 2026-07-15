//! Input — single-line text field with kill-ring, undo, paste, grapheme cursor.
//!
//! Port of `packages/tui/src/components/input.ts`.
//!
//! # Cursor index basis
//!
//! TypeScript stores `cursor` as a JS string index (UTF-16 code units). Rust
//! `str` indices are UTF-8 bytes. We store the cursor as a **UTF-16 code-unit
//! offset** matching JS semantics, converting to UTF-8 byte offsets via
//! [`utf16_to_byte`] / [`byte_to_utf16`] when slicing. Grapheme moves still
//! advance by whole grapheme clusters (measured in UTF-16 units of the
//! cluster), matching the TS `segmenter.segment` + `segment.length` pattern.

use unicode_segmentation::UnicodeSegmentation;

use crate::component::{Component, Focusable, RenderStatus};
use crate::keybindings::get_keybindings;
use crate::keys::decode_printable_key;
use crate::kill_ring::KillRing;
use crate::line::{CURSOR_MARKER, Line};
use crate::undo_stack::UndoStack;
use crate::util::{extract_ansi_code, grapheme_width, is_whitespace_char, visible_width};
use crate::word_navigation::{find_word_backward, find_word_forward};

#[derive(Debug, Clone)]
struct InputState {
    value: String,
    /// UTF-16 code-unit cursor (JS string index).
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Count UTF-16 code units in `s` (JS `s.length`).
#[must_use]
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Convert a UTF-16 code-unit offset into a UTF-8 byte offset in `s`.
/// Clamps to `s.len()` if past the end.
#[must_use]
pub fn utf16_to_byte(s: &str, utf16_offset: usize) -> usize {
    let mut units = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        if units >= utf16_offset {
            return byte_idx;
        }
        units += ch.len_utf16();
    }
    s.len()
}

/// Convert a UTF-8 byte offset into a UTF-16 code-unit offset.
#[must_use]
pub fn byte_to_utf16(s: &str, byte_offset: usize) -> usize {
    let byte_offset = byte_offset.min(s.len());
    utf16_len(&s[..byte_offset])
}

/// Slice `s` by UTF-16 code-unit range `[start, end)`.
fn slice_utf16(s: &str, start: usize, end: usize) -> &str {
    let b0 = utf16_to_byte(s, start);
    let b1 = utf16_to_byte(s, end);
    &s[b0..b1]
}

/// Slice from UTF-16 start to end of string.
fn slice_utf16_from(s: &str, start: usize) -> &str {
    &s[utf16_to_byte(s, start)..]
}

/// Slice from start of string to UTF-16 end.
fn slice_utf16_to(s: &str, end: usize) -> &str {
    &s[..utf16_to_byte(s, end)]
}

/// Extract a range of visible columns (no ANSI expected for Input value).
fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    if length == 0 {
        return String::new();
    }
    let end_col = start_col + length;
    let mut result = String::new();
    let mut current_col = 0usize;
    let mut i = 0usize;
    let mut pending_ansi = String::new();

    while i < line.len() {
        if let Some((code, len)) = extract_ansi_code(line, i) {
            if current_col >= start_col && current_col < end_col {
                result.push_str(code);
            } else if current_col < start_col {
                pending_ansi.push_str(code);
            }
            i += len;
            continue;
        }

        let mut text_end = i;
        while text_end < line.len() && extract_ansi_code(line, text_end).is_none() {
            text_end += 1;
        }
        let chunk = &line[i..text_end];
        for seg in chunk.graphemes(true) {
            let w = grapheme_width(seg);
            let in_range = current_col >= start_col && current_col < end_col;
            let fits = !strict || current_col + w <= end_col;
            if in_range && fits {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(seg);
            }
            current_col += w;
            if current_col >= end_col {
                break;
            }
        }
        i = text_end;
        if current_col >= end_col {
            break;
        }
    }
    result
}

/// Single-line text input with horizontal scrolling.
pub struct Input {
    value: String,
    /// Cursor as UTF-16 code-unit offset (JS string index).
    cursor: usize,
    pub on_submit: Option<Box<dyn FnMut(&str)>>,
    pub on_escape: Option<Box<dyn FnMut()>>,
    focused: bool,
    paste_buffer: String,
    is_in_paste: bool,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo_stack: UndoStack<InputState>,
    cached: Vec<Line>,
}

impl Input {
    #[must_use]
    pub fn new() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            on_submit: None,
            on_escape: None,
            focused: false,
            paste_buffer: String::new(),
            is_in_paste: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo_stack: UndoStack::new(),
            cached: Vec::new(),
        }
    }

    #[must_use]
    pub fn get_value(&self) -> &str {
        &self.value
    }

    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        let max = utf16_len(&self.value);
        self.cursor = self.cursor.min(max);
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(&InputState {
            value: self.value.clone(),
            cursor: self.cursor,
        });
    }

    fn undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.value = snapshot.value;
            self.cursor = snapshot.cursor;
            self.last_action = None;
        }
    }

    fn insert_character(&mut self, ch: &str) {
        if is_whitespace_char(ch) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);

        let byte = utf16_to_byte(&self.value, self.cursor);
        self.value.insert_str(byte, ch);
        self.cursor += utf16_len(ch);
    }

    fn handle_backspace(&mut self) {
        self.last_action = None;
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let before = slice_utf16_to(&self.value, self.cursor);
        let last = before.graphemes(true).next_back().unwrap_or("");
        let g_units = utf16_len(last).max(1);
        let new_cursor = self.cursor.saturating_sub(g_units);
        let b0 = utf16_to_byte(&self.value, new_cursor);
        let b1 = utf16_to_byte(&self.value, self.cursor);
        self.value.replace_range(b0..b1, "");
        self.cursor = new_cursor;
    }

    fn handle_forward_delete(&mut self) {
        self.last_action = None;
        let total = utf16_len(&self.value);
        if self.cursor >= total {
            return;
        }
        self.push_undo();
        let after = slice_utf16_from(&self.value, self.cursor);
        let first = after.graphemes(true).next().unwrap_or("");
        let g_units = utf16_len(first).max(1);
        let b0 = utf16_to_byte(&self.value, self.cursor);
        let b1 = utf16_to_byte(&self.value, self.cursor + g_units);
        self.value.replace_range(b0..b1, "");
    }

    fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let deleted = slice_utf16_to(&self.value, self.cursor).to_owned();
        self.kill_ring
            .push(&deleted, true, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value = slice_utf16_from(&self.value, self.cursor).to_owned();
        self.cursor = 0;
    }

    fn delete_to_line_end(&mut self) {
        let total = utf16_len(&self.value);
        if self.cursor >= total {
            return;
        }
        self.push_undo();
        let deleted = slice_utf16_from(&self.value, self.cursor).to_owned();
        self.kill_ring
            .push(&deleted, false, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value = slice_utf16_to(&self.value, self.cursor).to_owned();
    }

    fn delete_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();

        let old_cursor = self.cursor;
        self.move_word_backwards();
        let delete_from = self.cursor;
        self.cursor = old_cursor;

        let deleted = slice_utf16(&self.value, delete_from, self.cursor).to_owned();
        self.kill_ring.push(&deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);

        let b0 = utf16_to_byte(&self.value, delete_from);
        let b1 = utf16_to_byte(&self.value, self.cursor);
        self.value.replace_range(b0..b1, "");
        self.cursor = delete_from;
    }

    fn delete_word_forward(&mut self) {
        let total = utf16_len(&self.value);
        if self.cursor >= total {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();

        let old_cursor = self.cursor;
        self.move_word_forwards();
        let delete_to = self.cursor;
        self.cursor = old_cursor;

        let deleted = slice_utf16(&self.value, self.cursor, delete_to).to_owned();
        self.kill_ring.push(&deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);

        let b0 = utf16_to_byte(&self.value, self.cursor);
        let b1 = utf16_to_byte(&self.value, delete_to);
        self.value.replace_range(b0..b1, "");
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_owned) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.push_undo();
        let byte = utf16_to_byte(&self.value, self.cursor);
        self.value.insert_str(byte, &text);
        self.cursor += utf16_len(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();

        let prev = self.kill_ring.peek().unwrap_or("").to_owned();
        let prev_units = utf16_len(&prev);
        if self.cursor >= prev_units {
            let start = self.cursor - prev_units;
            let b0 = utf16_to_byte(&self.value, start);
            let b1 = utf16_to_byte(&self.value, self.cursor);
            self.value.replace_range(b0..b1, "");
            self.cursor = start;
        }

        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or("").to_owned();
        let byte = utf16_to_byte(&self.value, self.cursor);
        self.value.insert_str(byte, &text);
        self.cursor += utf16_len(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn move_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.last_action = None;
        // word_navigation uses UTF-16 code-unit offsets (JS string indices).
        self.cursor = find_word_backward(&self.value, self.cursor, None);
    }

    fn move_word_forwards(&mut self) {
        let total = utf16_len(&self.value);
        if self.cursor >= total {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_forward(&self.value, self.cursor, None);
    }

    fn handle_paste(&mut self, pasted: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = pasted
            .replace("\r\n", "")
            .replace(['\r', '\n'], "")
            .replace('\t', "    ");
        let byte = utf16_to_byte(&self.value, self.cursor);
        self.value.insert_str(byte, &clean);
        self.cursor += utf16_len(&clean);
    }

    fn move_cursor_left_grapheme(&mut self) {
        self.last_action = None;
        if self.cursor == 0 {
            return;
        }
        let before = slice_utf16_to(&self.value, self.cursor);
        let last = before.graphemes(true).next_back().unwrap_or("");
        let g_units = utf16_len(last).max(1);
        self.cursor = self.cursor.saturating_sub(g_units);
    }

    fn move_cursor_right_grapheme(&mut self) {
        self.last_action = None;
        let total = utf16_len(&self.value);
        if self.cursor >= total {
            return;
        }
        let after = slice_utf16_from(&self.value, self.cursor);
        let first = after.graphemes(true).next().unwrap_or("");
        let g_units = utf16_len(first).max(1);
        self.cursor = (self.cursor + g_units).min(total);
    }

    fn has_control_chars(data: &str) -> bool {
        data.chars().any(|ch| {
            let code = ch as u32;
            code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
        })
    }

    fn process_input(&mut self, mut data: String) {
        // Bracketed paste start
        if data.contains("\x1b[200~") {
            self.is_in_paste = true;
            self.paste_buffer.clear();
            data = data.replace("\x1b[200~", "");
        }

        if self.is_in_paste {
            self.paste_buffer.push_str(&data);
            if let Some(end_index) = self.paste_buffer.find("\x1b[201~") {
                let paste_content = self.paste_buffer[..end_index].to_owned();
                self.handle_paste(&paste_content);
                self.is_in_paste = false;
                let remaining = self.paste_buffer[end_index + 6..].to_owned();
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.process_input(remaining);
                }
            }
            return;
        }

        let kb = get_keybindings();

        if kb.matches(&data, "tui.select.cancel") {
            if let Some(cb) = &mut self.on_escape {
                cb();
            }
            return;
        }
        if kb.matches(&data, "tui.editor.undo") {
            self.undo();
            return;
        }
        if kb.matches(&data, "tui.input.submit") || data == "\n" {
            if let Some(cb) = &mut self.on_submit {
                cb(&self.value);
            }
            return;
        }
        if kb.matches(&data, "tui.editor.deleteCharBackward") {
            self.handle_backspace();
            return;
        }
        if kb.matches(&data, "tui.editor.deleteCharForward") {
            self.handle_forward_delete();
            return;
        }
        if kb.matches(&data, "tui.editor.deleteWordBackward") {
            self.delete_word_backwards();
            return;
        }
        if kb.matches(&data, "tui.editor.deleteWordForward") {
            self.delete_word_forward();
            return;
        }
        if kb.matches(&data, "tui.editor.deleteToLineStart") {
            self.delete_to_line_start();
            return;
        }
        if kb.matches(&data, "tui.editor.deleteToLineEnd") {
            self.delete_to_line_end();
            return;
        }
        if kb.matches(&data, "tui.editor.yank") {
            self.yank();
            return;
        }
        if kb.matches(&data, "tui.editor.yankPop") {
            self.yank_pop();
            return;
        }
        if kb.matches(&data, "tui.editor.cursorLeft") {
            self.move_cursor_left_grapheme();
            return;
        }
        if kb.matches(&data, "tui.editor.cursorRight") {
            self.move_cursor_right_grapheme();
            return;
        }
        if kb.matches(&data, "tui.editor.cursorLineStart") {
            self.last_action = None;
            self.cursor = 0;
            return;
        }
        if kb.matches(&data, "tui.editor.cursorLineEnd") {
            self.last_action = None;
            self.cursor = utf16_len(&self.value);
            return;
        }
        if kb.matches(&data, "tui.editor.cursorWordLeft") {
            self.move_word_backwards();
            return;
        }
        if kb.matches(&data, "tui.editor.cursorWordRight") {
            self.move_word_forwards();
            return;
        }

        if let Some(printable) = decode_printable_key(&data) {
            self.insert_character(&printable);
            return;
        }

        if !Self::has_control_chars(&data) {
            self.insert_character(&data);
        }
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Input {
    fn render(&mut self, width: u16) -> &[Line] {
        let prompt = "> ";
        let prompt_len = prompt.len(); // ASCII
        let available = (width as usize).saturating_sub(prompt_len);

        if available == 0 {
            self.cached = vec![Line::plain(prompt)];
            return &self.cached;
        }

        let total_width = visible_width(&self.value);
        let visible_text: String;
        let cursor_display: usize; // UTF-16 offset into visible_text

        if total_width < available {
            visible_text = self.value.clone();
            cursor_display = self.cursor;
        } else {
            let scroll_width = if self.cursor == utf16_len(&self.value) {
                available.saturating_sub(1)
            } else {
                available
            };
            let cursor_col = visible_width(slice_utf16_to(&self.value, self.cursor));

            if scroll_width > 0 {
                let half = scroll_width / 2;
                let start_col = if cursor_col < half {
                    0
                } else if cursor_col > total_width.saturating_sub(half) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half)
                };

                visible_text = slice_by_column(&self.value, start_col, scroll_width, true);
                let before_cursor = slice_by_column(
                    &self.value,
                    start_col,
                    cursor_col.saturating_sub(start_col),
                    true,
                );
                cursor_display = utf16_len(&before_cursor);
            } else {
                visible_text = String::new();
                cursor_display = 0;
            }
        }

        // Build line with fake reverse-video cursor + optional hardware marker.
        let after_slice = slice_utf16_from(&visible_text, cursor_display);
        let at_cursor = after_slice.graphemes(true).next().unwrap_or(" ").to_owned();
        let before_cursor = slice_utf16_to(&visible_text, cursor_display);
        let after_cursor = {
            let skip = utf16_len(&at_cursor);
            slice_utf16_from(&visible_text, cursor_display + skip).to_owned()
        };

        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let cursor_char = format!("\x1b[7m{at_cursor}\x1b[27m");
        let text_with_cursor = format!("{before_cursor}{marker}{cursor_char}{after_cursor}");

        let visual_length = visible_width(&text_with_cursor);
        let padding = " ".repeat(available.saturating_sub(visual_length));
        let line = format!("{prompt}{text_with_cursor}{padding}");

        self.cached = vec![Line::from_ansi(&line)];
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn handle_input(&mut self, data: &str) {
        self.process_input(data.to_owned());
    }

    fn last_render_status(&self) -> RenderStatus {
        // Always re-render (TS has no cache).
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for Input {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{Component, Focusable};

    #[test]
    fn utf16_helpers_surrogate_pair() {
        // U+1F600 😀 is one char, two UTF-16 units, four UTF-8 bytes.
        let s = "a😀b";
        assert_eq!(utf16_len(s), 1 + 2 + 1);
        assert_eq!(byte_to_utf16(s, s.len()), utf16_len(s));
        // After 'a' (1 unit) sits the emoji.
        let after_a = utf16_to_byte(s, 1);
        assert_eq!(&s[..after_a], "a");
        let after_emoji = utf16_to_byte(s, 3);
        assert_eq!(&s[..after_emoji], "a😀");
    }

    #[test]
    fn insert_and_grapheme_backspace_over_emoji() {
        let mut input = Input::new();
        input.handle_input("a");
        input.handle_input("😀");
        input.handle_input("b");
        assert_eq!(input.get_value(), "a😀b");
        assert_eq!(input.cursor, utf16_len("a😀b"));

        // Backspace deletes grapheme 'b'
        input.handle_input("\x7f");
        assert_eq!(input.get_value(), "a😀");
        // Backspace deletes whole emoji grapheme
        input.handle_input("\x7f");
        assert_eq!(input.get_value(), "a");
        assert_eq!(input.cursor, 1);
    }

    #[test]
    fn bracketed_paste_split_chunks() {
        let mut input = Input::new();
        input.handle_input("\x1b[200~hel");
        assert_eq!(input.get_value(), ""); // still buffering
        input.handle_input("lo\x1b[201~");
        assert_eq!(input.get_value(), "hello");
    }

    #[test]
    fn paste_strips_newlines_and_expands_tabs() {
        let mut input = Input::new();
        input.handle_input("\x1b[200~a\nb\tc\x1b[201~");
        assert_eq!(input.get_value(), "ab    c");
    }

    #[test]
    fn always_render_status_changed() {
        let mut input = Input::new();
        input.set_focused(true);
        let _ = input.render(40);
        assert_eq!(input.last_render_status(), RenderStatus::Changed);
        let line = input.render(40)[0].to_ansi();
        assert!(line.contains(CURSOR_MARKER));
        assert!(line.starts_with("> "));
    }

    #[test]
    fn left_right_move_by_grapheme() {
        let mut input = Input::new();
        input.set_value("a😀b");
        input.cursor = utf16_len("a😀b");
        input.handle_input("\x1b[D"); // left
        // Should land before 'b' (after emoji)
        assert_eq!(input.cursor, utf16_len("a😀"));
        input.handle_input("\x1b[D"); // left over emoji
        assert_eq!(input.cursor, 1); // after 'a'
        input.handle_input("\x1b[C"); // right over emoji
        assert_eq!(input.cursor, utf16_len("a😀"));
    }
}
