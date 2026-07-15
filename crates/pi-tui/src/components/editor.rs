//! Multiline editor — port of `packages/tui/src/components/editor.ts`.
//!
//! Cursor columns are JavaScript UTF-16 code-unit offsets.  All editing is
//! grapheme-aware even though storage is UTF-8.

use std::collections::HashMap;

use unicode_segmentation::UnicodeSegmentation;

use crate::autocomplete::{AutocompleteItem, AutocompleteProvider, SuggestionOptions};
use crate::component::{Component, Focusable, RenderStatus};
use crate::components::input::{byte_to_utf16, utf16_len, utf16_to_byte};
use crate::components::select_list::{SelectItem, SelectList, SelectListTheme};
use crate::keybindings::get_keybindings;
use crate::keys::decode_printable_key;
use crate::kill_ring::KillRing;
use crate::line::{CURSOR_MARKER, Line};
use crate::undo_stack::UndoStack;
use crate::util::{CJK_BREAK, is_whitespace_char, visible_width};
use crate::word_navigation::{find_word_backward, find_word_forward};

const PASTE_PREFIX: &str = "[paste #";

/// Minimal terminal services required by [`Editor`].
pub trait EditorTui {
    fn request_render(&self);
    fn terminal_rows(&self) -> u16;
}

/// An unstyled editor theme. Styling belongs at the TUI boundary.
#[derive(Default)]
pub struct EditorTheme;

/// Construction options.
#[derive(Debug, Clone, Copy, Default)]
pub struct EditorOptions {
    pub padding_x: usize,
    pub autocomplete_max_visible: usize,
}

/// A line fragment returned by [`word_wrap_line`]. Indices are UTF-16 offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

fn is_marker(s: &str) -> bool {
    s.starts_with(PASTE_PREFIX)
        && s.ends_with(']')
        && (s.contains(" chars]") || s.contains(" lines]"))
}

fn graphemes_with_markers(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut byte = 0;
    while byte < text.len() {
        if text[byte..].starts_with(PASTE_PREFIX)
            && let Some(end) = text[byte..].find(']') {
                let marker = &text[byte..byte + end + 1];
                if is_marker(marker) {
                    out.push((byte_to_utf16(text, byte), marker.to_owned()));
                    byte += end + 1;
                    continue;
                }
            }
        let rest = &text[byte..];
        let g = rest.graphemes(true).next().expect("nonempty");
        out.push((byte_to_utf16(text, byte), g.to_owned()));
        byte += g.len();
    }
    out
}

/// Pi's word-aware wrap algorithm (`editor.ts:114-206`).
pub fn word_wrap_line(line: &str, max_width: usize) -> Vec<TextChunk> {
    word_wrap_line_inner(line, max_width, false)
}

fn word_wrap_line_inner(line: &str, max_width: usize, atomic_markers: bool) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }
    if visible_width(line) <= max_width {
        return vec![TextChunk {
            text: line.to_owned(),
            start_index: 0,
            end_index: utf16_len(line),
        }];
    }
    let segs = if atomic_markers {
        graphemes_with_markers(line)
    } else {
        line.grapheme_indices(true)
            .map(|(byte, g)| (byte_to_utf16(line, byte), g.to_owned()))
            .collect()
    };
    let mut chunks = Vec::new();
    let mut width = 0usize;
    let mut start = 0usize;
    let mut wrap: Option<(usize, usize)> = None;
    for (i, (index, grapheme)) in segs.iter().enumerate() {
        let gwidth = visible_width(grapheme);
        if width + gwidth > max_width {
            if let Some((wrap_index, wrap_width)) = wrap {
                if width - wrap_width + gwidth <= max_width {
                    chunks.push(chunk(line, start, wrap_index));
                    start = wrap_index;
                    width -= wrap_width;
                } else if start < *index {
                    chunks.push(chunk(line, start, *index));
                    start = *index;
                    width = 0;
                }
            } else if start < *index {
                chunks.push(chunk(line, start, *index));
                start = *index;
                width = 0;
            }
            wrap = None;
        }
        // Oversized atomic marker: split only for layout, retaining atomic editor behavior.
        if gwidth > max_width && atomic_markers && is_marker(grapheme) {
            let mut pieces = Vec::new();
            let mut piece = String::new();
            let mut piece_width = 0;
            let mut piece_start = 0;
            for g in grapheme.graphemes(true) {
                let gw = visible_width(g);
                if !piece.is_empty() && piece_width + gw > max_width {
                    let end = piece_start + utf16_len(&piece);
                    pieces.push(TextChunk {
                        text: std::mem::take(&mut piece),
                        start_index: piece_start,
                        end_index: end,
                    });
                    piece_start = end;
                    piece_width = 0;
                }
                piece.push_str(g);
                piece_width += gw;
            }
            if !piece.is_empty() {
                pieces.push(TextChunk {
                    end_index: piece_start + utf16_len(&piece),
                    start_index: piece_start,
                    text: piece,
                });
            }
            let last = pieces.pop().expect("marker has graphemes");
            for sub in pieces {
                chunks.push(TextChunk {
                    text: sub.text,
                    start_index: *index + sub.start_index,
                    end_index: *index + sub.end_index,
                });
            }
            start = *index + last.start_index;
            width = visible_width(&last.text);
            wrap = None;
            continue;
        }
        width += gwidth;
        let ws = !(atomic_markers && is_marker(grapheme)) && is_whitespace_char(grapheme);
        if let Some((_, next)) = segs.get(i + 1) {
            let next_ws = !(atomic_markers && is_marker(next)) && is_whitespace_char(next);
            if (ws && !next_ws)
                || (!ws
                    && !next_ws
                    && (!(atomic_markers && is_marker(grapheme))
                        && (CJK_BREAK.is_match(grapheme) || CJK_BREAK.is_match(next))))
            {
                let next_index = segs[i + 1].0;
                wrap = Some((next_index, width));
            }
        }
    }
    chunks.push(chunk(line, start, utf16_len(line)));
    chunks
}

/// Alias matching the TypeScript export spelling.
#[allow(non_snake_case)]
pub fn wordWrapLine(line: &str, max_width: usize) -> Vec<TextChunk> {
    word_wrap_line(line, max_width)
}

fn chunk(line: &str, start: usize, end: usize) -> TextChunk {
    TextChunk {
        text: line[utf16_to_byte(line, start)..utf16_to_byte(line, end)].to_owned(),
        start_index: start,
        end_index: end,
    }
}

#[derive(Debug, Clone)]
struct State {
    lines: Vec<String>,
    line: usize,
    col: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Kill,
    Yank,
    Word,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Jump {
    Forward,
    Backward,
}

/// Grapheme-aware multiline terminal editor.
pub struct Editor<'a> {
    tui: &'a dyn EditorTui,
    state: State,
    focused: bool,
    padding_x: usize,
    last_width: usize,
    history: Vec<String>,
    history_index: isize,
    history_draft: Option<State>,
    kill_ring: KillRing,
    last_action: Option<Action>,
    undo: UndoStack<State>,
    pastes: HashMap<usize, String>,
    paste_counter: usize,
    in_paste: bool,
    paste_buffer: String,
    jump: Option<Jump>,
    preferred_col: Option<usize>,
    provider: Option<Box<dyn AutocompleteProvider>>,
    autocomplete: Option<SelectList>,
    autocomplete_prefix: String,
    autocomplete_force: bool,
    cached: Vec<Line>,
    pub on_submit: Option<Box<dyn FnMut(String)>>,
    pub on_change: Option<Box<dyn FnMut(String)>>,
    pub disable_submit: bool,
}

impl<'a> Editor<'a> {
    pub fn new(tui: &'a dyn EditorTui, _theme: EditorTheme) -> Self {
        Self::with_options(tui, EditorTheme, EditorOptions::default())
    }
    pub fn with_options(
        tui: &'a dyn EditorTui,
        _theme: EditorTheme,
        options: EditorOptions,
    ) -> Self {
        Self {
            tui,
            state: State {
                lines: vec![String::new()],
                line: 0,
                col: 0,
            },
            focused: false,
            padding_x: options.padding_x,
            last_width: 80,
            history: vec![],
            history_index: -1,
            history_draft: None,
            kill_ring: KillRing::new(),
            last_action: None,
            undo: UndoStack::new(),
            pastes: HashMap::new(),
            paste_counter: 0,
            in_paste: false,
            paste_buffer: String::new(),
            jump: None,
            preferred_col: None,
            provider: None,
            autocomplete: None,
            autocomplete_prefix: String::new(),
            autocomplete_force: false,
            cached: vec![],
            on_submit: None,
            on_change: None,
            disable_submit: false,
        }
    }
    pub fn get_text(&self) -> String {
        self.state.lines.join("\n")
    }
    pub fn get_expanded_text(&self) -> String {
        self.expand_markers(&self.get_text())
    }
    pub fn get_lines(&self) -> Vec<String> {
        self.state.lines.clone()
    }
    pub fn get_cursor(&self) -> (usize, usize) {
        (self.state.line, self.state.col)
    }
    pub fn set_text(&mut self, text: &str) {
        if self.get_text() != normalize(text) {
            self.push_undo();
        }
        self.exit_history();
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_inner(text, false, false);
    }
    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_undo();
        self.last_action = None;
        self.exit_history();
        self.insert_text_inner(text);
        self.changed();
    }
    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if !trimmed.is_empty() && self.history.first().is_none_or(|x| x != trimmed) {
            self.history.insert(0, trimmed.to_owned());
            self.history.truncate(100);
        }
    }
    pub fn set_autocomplete_provider(&mut self, provider: Box<dyn AutocompleteProvider>) {
        self.provider = Some(provider);
        self.cancel_autocomplete();
    }
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete.is_some()
    }
    pub fn get_padding_x(&self) -> usize {
        self.padding_x
    }
    pub fn set_padding_x(&mut self, x: usize) {
        self.padding_x = x;
        self.tui.request_render();
    }

    fn line(&self) -> &str {
        &self.state.lines[self.state.line]
    }
    fn set_col(&mut self, col: usize) {
        self.state.col = col.min(utf16_len(self.line()));
        self.preferred_col = None;
    }
    fn push_undo(&mut self) {
        self.undo.push(&self.state);
    }
    fn changed(&mut self) {
        let text = self.get_text();
        if let Some(cb) = &mut self.on_change {
            cb(text);
        }
        self.tui.request_render();
    }
    fn exit_history(&mut self) {
        self.history_index = -1;
        self.history_draft = None;
    }
    fn set_text_inner(&mut self, text: &str, start: bool, notify: bool) {
        self.state.lines = normalize(text).split('\n').map(ToOwned::to_owned).collect();
        if self.state.lines.is_empty() {
            self.state.lines.push(String::new());
        }
        self.state.line = if start { 0 } else { self.state.lines.len() - 1 };
        self.state.col = if start { 0 } else { utf16_len(self.line()) };
        self.preferred_col = None;
        if notify {
            self.changed();
        }
    }
    fn insert_text_inner(&mut self, text: &str) {
        let normalized = normalize(text);
        let b = utf16_to_byte(self.line(), self.state.col);
        let before = self.line()[..b].to_owned();
        let after = self.line()[b..].to_owned();
        let parts: Vec<_> = normalized.split('\n').collect();
        if parts.len() == 1 {
            self.state.lines[self.state.line] = format!("{before}{normalized}{after}");
            self.state.col += utf16_len(&normalized);
        } else {
            self.state.lines[self.state.line] = format!("{before}{}", parts[0]);
            for (n, part) in parts.iter().enumerate().skip(1) {
                self.state.lines.insert(
                    self.state.line + n,
                    if n + 1 == parts.len() {
                        format!("{part}{after}")
                    } else {
                        (*part).to_owned()
                    },
                );
            }
            self.state.line += parts.len() - 1;
            self.state.col = utf16_len(parts.last().unwrap());
        }
        self.preferred_col = None;
    }
    fn expand_markers(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (id, value) in &self.pastes {
            out = out.replace(
                &format!("[paste #{id} +{} lines]", value.split('\n').count()),
                value,
            );
            out = out.replace(&format!("[paste #{id} {} chars]", utf16_len(value)), value);
        }
        out
    }
    fn marker_id(&self, text: &str) -> Option<usize> {
        let id = text
            .strip_prefix(PASTE_PREFIX)?
            .split_once(' ')?
            .0
            .parse::<usize>()
            .ok()?;
        self.pastes.contains_key(&id).then_some(id)
    }
    fn atomic_before_cursor(&self) -> Option<(usize, usize)> {
        let b = utf16_to_byte(self.line(), self.state.col);
        let before = &self.line()[..b];
        let start = before.rfind(PASTE_PREFIX)?;
        let marker = &before[start..];
        marker.ends_with(']').then_some(())?;
        self.marker_id(marker)
            .map(|id| (byte_to_utf16(self.line(), start), id))
    }
    fn atomic_at_cursor(&self) -> Option<(usize, usize)> {
        let b = utf16_to_byte(self.line(), self.state.col);
        let rest = &self.line()[b..];
        let end = rest.find(']')? + 1;
        let marker = &rest[..end];
        self.marker_id(marker)
            .map(|id| (utf16_len(marker), id))
    }
    fn remove_paste(&mut self, id: usize) {
        self.pastes.remove(&id);
        // Keep marker IDs contiguous exactly like the TypeScript editor.
        let mut moved = HashMap::new();
        for (old, value) in std::mem::take(&mut self.pastes) {
            let new = if old > id { old - 1 } else { old };
            moved.insert(new, value);
        }
        self.pastes = moved;
        for line in &mut self.state.lines {
            let mut replacements = Vec::new();
            let mut at = 0;
            while let Some(found) = line[at..].find(PASTE_PREFIX) {
                let start = at + found;
                let Some(close) = line[start..].find(']') else { break };
                let end = start + close + 1;
                let marker = &line[start..end];
                if let Some(number) = marker
                    .strip_prefix(PASTE_PREFIX)
                    .and_then(|s| s.split_once(' '))
                    .and_then(|(n, _)| n.parse::<usize>().ok())
                    && number > id
                {
                    replacements.push((start, end, marker.replacen(&format!("#{number}"), &format!("#{}", number - 1), 1)));
                }
                at = end;
            }
            for (start, end, replacement) in replacements.into_iter().rev() {
                line.replace_range(start..end, &replacement);
            }
        }
        self.paste_counter = self.pastes.keys().copied().max().unwrap_or(0);
    }
    fn navigate_history(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = self.history_index + if up { 1 } else { -1 };
        if next < -1 || (next >= 0 && next as usize >= self.history.len()) {
            return;
        }
        if self.history_index == -1 && next >= 0 {
            self.push_undo();
            self.history_draft = Some(self.state.clone());
        }
        self.history_index = next;
        if next == -1 {
            if let Some(draft) = self.history_draft.take() {
                self.state = draft;
                self.changed();
            }
        } else {
            self.set_text_inner(&self.history[next as usize].clone(), up, true);
        }
    }
    fn insert_char(&mut self, text: &str, coalesce: bool) {
        self.exit_history();
        let ws = is_whitespace_char(text);
        if !coalesce || ws || self.last_action != Some(Action::Word) {
            self.push_undo();
        }
        self.insert_text_inner(text);
        self.last_action = Some(Action::Word);
        self.changed();
        self.trigger_autocomplete(false);
    }
    fn newline(&mut self) {
        self.exit_history();
        self.last_action = None;
        self.push_undo();
        self.insert_text_inner("\n");
        self.changed();
    }
    fn backspace(&mut self) {
        self.exit_history();
        self.last_action = None;
        if self.state.col > 0 {
            self.push_undo();
            let atomic = self.atomic_before_cursor();
            let b = utf16_to_byte(self.line(), self.state.col);
            let (len, removed) = if let Some((start, id)) = atomic {
                (self.state.col - start, Some(id))
            } else {
                let before = self.line()[..b].to_owned();
                (before.graphemes(true).next_back().map(utf16_len).unwrap_or(1), None)
            };
            let begin = utf16_to_byte(self.line(), self.state.col - len);
            self.state.lines[self.state.line].replace_range(begin..b, "");
            self.state.col -= len;
            if let Some(id) = removed {
                self.remove_paste(id);
            }
        } else if self.state.line > 0 {
            self.push_undo();
            let current = self.state.lines.remove(self.state.line);
            self.state.line -= 1;
            self.state.col = utf16_len(self.line());
            self.state.lines[self.state.line].push_str(&current);
        }
        self.changed();
    }
    fn forward_delete(&mut self) {
        self.exit_history();
        self.last_action = None;
        if self.state.col < utf16_len(self.line()) {
            self.push_undo();
            let atomic = self.atomic_at_cursor();
            let b = utf16_to_byte(self.line(), self.state.col);
            let (len, removed) = if let Some((len, id)) = atomic {
                (len, Some(id))
            } else {
                (self.line()[b..].graphemes(true).next().map(utf16_len).unwrap_or(1), None)
            };
            let end = utf16_to_byte(self.line(), self.state.col + len);
            self.state.lines[self.state.line].replace_range(b..end, "");
            if let Some(id) = removed {
                self.remove_paste(id);
            }
        } else if self.state.line + 1 < self.state.lines.len() {
            self.push_undo();
            let next = self.state.lines.remove(self.state.line + 1);
            self.state.lines[self.state.line].push_str(&next);
        }
        self.changed();
    }
    fn kill_range(&mut self, start: usize, end: usize, prepend: bool) {
        if start == end {
            return;
        }
        self.push_undo();
        let b0 = utf16_to_byte(self.line(), start);
        let b1 = utf16_to_byte(self.line(), end);
        let deleted = self.line()[b0..b1].to_owned();
        let accumulate = self.last_action == Some(Action::Kill);
        self.kill_ring.push(&deleted, prepend, accumulate);
        self.state.lines[self.state.line].replace_range(b0..b1, "");
        self.state.col = start;
        self.last_action = Some(Action::Kill);
        self.changed();
    }
    fn kill_start(&mut self) {
        if self.state.col > 0 {
            self.kill_range(0, self.state.col, true);
        } else if self.state.line > 0 {
            self.push_undo();
            let cur = self.state.lines.remove(self.state.line);
            self.state.line -= 1;
            self.state.col = utf16_len(self.line());
            self.state.lines[self.state.line].push_str(&cur);
            self.kill_ring
                .push("\n", true, self.last_action == Some(Action::Kill));
            self.last_action = Some(Action::Kill);
            self.changed();
        }
    }
    fn kill_end(&mut self) {
        let end = utf16_len(self.line());
        if self.state.col < end {
            self.kill_range(self.state.col, end, false);
        } else if self.state.line + 1 < self.state.lines.len() {
            self.push_undo();
            let next = self.state.lines.remove(self.state.line + 1);
            self.state.lines[self.state.line].push_str(&next);
            self.kill_ring
                .push("\n", false, self.last_action == Some(Action::Kill));
            self.last_action = Some(Action::Kill);
            self.changed();
        }
    }
    fn kill_word_back(&mut self) {
        if self.state.col == 0 {
            self.kill_start();
        } else {
            let start = find_word_backward(self.line(), self.state.col, None);
            self.kill_range(start, self.state.col, true);
        }
    }
    fn kill_word_forward(&mut self) {
        let end = utf16_len(self.line());
        if self.state.col == end {
            self.kill_end();
        } else {
            let stop = find_word_forward(self.line(), self.state.col, None);
            self.kill_range(self.state.col, stop, false);
        }
    }
    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(ToOwned::to_owned) else {
            return;
        };
        self.push_undo();
        self.insert_text_inner(&text);
        self.last_action = Some(Action::Yank);
        self.changed();
    }
    fn yank_pop(&mut self) {
        if self.last_action != Some(Action::Yank) || self.kill_ring.len() < 2 {
            return;
        }
        let old = self.kill_ring.peek().unwrap().to_owned();
        self.push_undo();
        self.delete_inserted(&old);
        self.kill_ring.rotate();
        let replacement = self.kill_ring.peek().unwrap().to_owned();
        self.insert_text_inner(&replacement);
        self.last_action = Some(Action::Yank);
        self.changed();
    }
    fn delete_inserted(&mut self, text: &str) {
        let parts: Vec<_> = text.split('\n').collect();
        if parts.len() == 1 {
            let start = self.state.col - utf16_len(text);
            let b0 = utf16_to_byte(self.line(), start);
            let b1 = utf16_to_byte(self.line(), self.state.col);
            self.state.lines[self.state.line].replace_range(b0..b1, "");
            self.state.col = start;
        } else {
            let first = self.state.line - parts.len() + 1;
            let start = utf16_len(&self.state.lines[first]) - utf16_len(parts[0]);
            let suffix = self.state.lines[self.state.line]
                [utf16_to_byte(self.line(), self.state.col)..]
                .to_owned();
            let prefix = self.state.lines[first][..utf16_to_byte(&self.state.lines[first], start)]
                .to_owned();
            self.state
                .lines
                .splice(first..=self.state.line, [format!("{prefix}{suffix}")]);
            self.state.line = first;
            self.state.col = start;
        }
    }
    fn undo(&mut self) {
        self.exit_history();
        if let Some(state) = self.undo.pop() {
            self.state = state;
            self.last_action = None;
            self.changed();
        }
    }
    fn move_horiz(&mut self, right: bool) {
        self.last_action = None;
        self.preferred_col = None;
        if right {
            if let Some((len, _)) = self.atomic_at_cursor() {
                self.state.col += len;
            } else if self.state.col < utf16_len(self.line()) {
                let b = utf16_to_byte(self.line(), self.state.col);
                self.state.col += self.line()[b..].graphemes(true).next().map(utf16_len).unwrap_or(1);
            } else if self.state.line + 1 < self.state.lines.len() {
                self.state.line += 1;
                self.state.col = 0;
            }
        } else if let Some((start, _)) = self.atomic_before_cursor() {
            self.state.col = start;
        } else if self.state.col > 0 {
            let b = utf16_to_byte(self.line(), self.state.col);
            self.state.col -= self.line()[..b].graphemes(true).next_back().map(utf16_len).unwrap_or(1);
        } else if self.state.line > 0 {
            self.state.line -= 1;
            self.state.col = utf16_len(self.line());
        }
    }
    fn move_vert(&mut self, down: bool) {
        self.last_action = None;
        let target = if down {
            self.state.line + 1
        } else {
            self.state.line.saturating_sub(1)
        };
        if target >= self.state.lines.len() || (!down && self.state.line == 0) {
            return;
        }
        let col = *self.preferred_col.get_or_insert(self.state.col);
        self.state.line = target;
        self.state.col = col.min(utf16_len(self.line()));
    }
    fn move_word(&mut self, forward: bool) {
        self.last_action = None;
        if forward {
            if self.state.col == utf16_len(self.line()) {
                self.move_horiz(true);
            } else {
                self.state.col = find_word_forward(self.line(), self.state.col, None);
            }
        } else if self.state.col == 0 && self.state.line > 0 {
            self.state.line -= 1;
            self.state.col = utf16_len(self.line());
        } else {
            self.state.col = find_word_backward(self.line(), self.state.col, None);
        }
    }
    fn jump_to(&mut self, wanted: &str, forward: bool) {
        self.last_action = None;
        if forward {
            for i in self.state.line..self.state.lines.len() {
                let from = if i == self.state.line {
                    self.state.col + 1
                } else {
                    0
                };
                let b = utf16_to_byte(&self.state.lines[i], from);
                if let Some(pos) = self.state.lines[i][b..].find(wanted) {
                    self.state.line = i;
                    self.state.col = byte_to_utf16(&self.state.lines[i], b + pos);
                    return;
                }
            }
        } else {
            for i in (0..=self.state.line).rev() {
                let line = &self.state.lines[i];
                let before = if i == self.state.line {
                    utf16_to_byte(line, self.state.col)
                } else {
                    line.len()
                };
                if let Some(pos) = line[..before].rfind(wanted) {
                    self.state.line = i;
                    self.state.col = byte_to_utf16(line, pos);
                    return;
                }
            }
        }
    }
    fn handle_paste(&mut self, text: &str) {
        // tmux can encode newlines inside a bracketed paste as CSI-u Ctrl+J.
        let mut decoded = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("\x1b[") {
            decoded.push_str(&rest[..start]);
            let suffix = &rest[start + 2..];
            if let Some(end) = suffix.find(";5u")
                && let Ok(cp) = suffix[..end].parse::<u32>()
                    && ((65..=90).contains(&cp) || (97..=122).contains(&cp)) {
                        decoded.push(char::from_u32(if cp >= 97 { cp - 96 } else { cp - 64 }).unwrap());
                        rest = &suffix[end + 3..];
                        continue;
                    }
            decoded.push_str("\x1b[");
            rest = suffix;
        }
        decoded.push_str(rest);
        let mut text = normalize(&decoded);
        text.retain(|c| c == '\n' || c >= ' ');
        let line_count = text.split('\n').count();
        if line_count > 10 || utf16_len(&text) > 1000 {
            self.paste_counter += 1;
            let id = self.paste_counter;
            let marker = if line_count > 10 {
                format!("[paste #{id} +{line_count} lines]")
            } else {
                format!("[paste #{id} {} chars]", utf16_len(&text))
            };
            self.pastes.insert(id, text);
            self.insert_text_at_cursor(&marker);
        } else {
            self.insert_text_at_cursor(&text);
        }
    }
    fn trigger_autocomplete(&mut self, force: bool) {
        let Some(provider) = self.provider.as_ref() else {
            return;
        };
        if force
            && !provider.should_trigger_file_completion(
                &self.state.lines,
                self.state.line,
                self.state.col,
            )
        {
            return;
        }
        let Some(s) = provider.get_suggestions(
            &self.state.lines,
            self.state.line,
            self.state.col,
            SuggestionOptions { force },
        ) else {
            self.cancel_autocomplete();
            return;
        };
        if s.items.is_empty() {
            self.cancel_autocomplete();
            return;
        }
        if force && s.items.len() == 1 {
            self.apply_completion(&s.items[0], &s.prefix);
            return;
        }
        let items = s
            .items
            .iter()
            .map(|x| {
                let mut y = SelectItem::new(&x.value, &x.label);
                y.description = x.description.clone();
                y
            })
            .collect();
        let mut list = SelectList::new(items, 5, SelectListTheme::identity(), Default::default());
        if let Some(n) = s
            .items
            .iter()
            .position(|x| x.value == s.prefix || x.value.starts_with(&s.prefix))
        {
            list.set_selected_index(n);
        }
        self.autocomplete = Some(list);
        self.autocomplete_prefix = s.prefix;
        self.autocomplete_force = force;
    }
    fn apply_completion(&mut self, item: &AutocompleteItem, prefix: &str) {
        let Some(provider) = self.provider.as_ref() else {
            return;
        };
        let applied = provider.apply_completion(
            &self.state.lines,
            self.state.line,
            self.state.col,
            item,
            prefix,
        );
        self.push_undo();
        self.state.lines = applied.lines;
        self.state.line = applied.cursor_line;
        self.state.col = applied.cursor_col;
        self.cancel_autocomplete();
        self.changed();
    }
    fn cancel_autocomplete(&mut self) {
        self.autocomplete = None;
        self.autocomplete_prefix.clear();
    }
    fn submit(&mut self) {
        if self.disable_submit {
            return;
        }
        let text = self.get_expanded_text().trim().to_owned();
        self.state = State {
            lines: vec![String::new()],
            line: 0,
            col: 0,
        };
        self.pastes.clear();
        self.undo.clear();
        self.last_action = None;
        self.exit_history();
        if let Some(cb) = &mut self.on_submit {
            cb(text);
        }
        self.changed();
    }

    /// Raw terminal input entry point.
    pub fn handle_input(&mut self, mut data: &str) {
        if let Some(jump) = self.jump {
            if data == "\x1d" || data == "\x1b\x1d" {
                self.jump = None;
                return;
            }
            if let Some(ch) = decode_printable_key(data).or_else(|| {
                data.chars()
                    .next()
                    .filter(|c| !c.is_control())
                    .map(|c| c.to_string())
            }) {
                self.jump = None;
                self.jump_to(&ch, jump == Jump::Forward);
                return;
            }
            self.jump = None;
        }
        if let Some(pos) = data.find("\x1b[200~") {
            self.in_paste = true;
            self.paste_buffer.clear();
            data = &data[pos + 6..];
        }
        if self.in_paste {
            self.paste_buffer.push_str(data);
            if let Some(end) = self.paste_buffer.find("\x1b[201~") {
                let content = self.paste_buffer[..end].to_owned();
                let rest = self.paste_buffer[end + 6..].to_owned();
                self.paste_buffer.clear();
                self.in_paste = false;
                self.handle_paste(&content);
                if !rest.is_empty() {
                    self.handle_input(&rest);
                }
            }
            return;
        }
        let kb = get_keybindings();
        if kb.matches(data, "tui.editor.undo") {
            self.undo();
        } else if kb.matches(data, "tui.editor.jumpForward") {
            self.jump = Some(Jump::Forward);
        } else if kb.matches(data, "tui.editor.jumpBackward") {
            self.jump = Some(Jump::Backward);
        } else if kb.matches(data, "tui.editor.deleteToLineStart") {
            self.kill_start();
        } else if kb.matches(data, "tui.editor.deleteToLineEnd") {
            self.kill_end();
        } else if kb.matches(data, "tui.editor.deleteWordBackward") {
            self.kill_word_back();
        } else if kb.matches(data, "tui.editor.deleteWordForward") {
            self.kill_word_forward();
        } else if kb.matches(data, "tui.editor.deleteCharBackward") {
            self.backspace();
        } else if kb.matches(data, "tui.editor.deleteCharForward") {
            self.forward_delete();
        } else if kb.matches(data, "tui.editor.yank") {
            self.yank();
        } else if kb.matches(data, "tui.editor.yankPop") {
            self.yank_pop();
        } else if kb.matches(data, "tui.editor.cursorLineStart") {
            self.last_action = None;
            self.set_col(0);
        } else if kb.matches(data, "tui.editor.cursorLineEnd") {
            self.last_action = None;
            self.set_col(utf16_len(self.line()));
        } else if kb.matches(data, "tui.editor.cursorWordLeft") {
            self.move_word(false);
        } else if kb.matches(data, "tui.editor.cursorWordRight") {
            self.move_word(true);
        } else if kb.matches(data, "tui.editor.cursorLeft") {
            self.move_horiz(false);
        } else if kb.matches(data, "tui.editor.cursorRight") {
            self.move_horiz(true);
        } else if kb.matches(data, "tui.editor.cursorUp") {
            if self.history_index >= 0 || (self.state.line == 0 && self.state.col == 0) {
                self.navigate_history(true);
            } else if self.state.line == 0 {
                self.set_col(0);
            } else {
                self.move_vert(false);
            }
        } else if kb.matches(data, "tui.editor.cursorDown") {
            if self.history_index >= 0 {
                self.navigate_history(false);
            } else if self.state.line + 1 == self.state.lines.len() {
                self.set_col(utf16_len(self.line()));
            } else {
                self.move_vert(true);
            }
        } else if kb.matches(data, "tui.input.tab") {
            if let Some(list) = &self.autocomplete {
                if let Some(selected) = list.get_selected_item() {
                    let item = AutocompleteItem {
                        value: selected.value.clone(),
                        label: selected.label.clone(),
                        description: selected.description.clone(),
                    };
                    let prefix = self.autocomplete_prefix.clone();
                    self.apply_completion(&item, &prefix);
                }
            } else {
                self.trigger_autocomplete(true);
            }
        } else if kb.matches(data, "tui.input.newLine") {
            self.newline();
        } else if kb.matches(data, "tui.input.submit") {
            if self.state.col > 0
                && self.line()[..utf16_to_byte(self.line(), self.state.col)].ends_with('\\')
            {
                self.backspace();
                self.newline();
            } else {
                self.submit();
            }
        } else if let Some(ch) = decode_printable_key(data) {
            self.insert_char(&ch, true);
        } else if !data.chars().any(char::is_control) {
            self.insert_char(data, true);
        }
    }
}

impl Component for Editor<'_> {
    fn render(&mut self, width: u16) -> &[Line] {
        let width = width as usize;
        let padding = self.padding_x.min(width.saturating_sub(1) / 2);
        let content = width.saturating_sub(padding * 2).max(1);
        let layout = if padding == 0 {
            content.saturating_sub(1).max(1)
        } else {
            content
        };
        self.last_width = layout;
        let mut rendered = vec![Line::plain("─".repeat(width))];
        for (line_index, logical) in self.state.lines.iter().enumerate() {
            for part in word_wrap_line_inner(logical, layout, true) {
                let has = line_index == self.state.line
                    && self.state.col >= part.start_index
                    && self.state.col <= part.end_index;
                let mut text = part.text;
                if has {
                    let offset = (self.state.col - part.start_index).min(utf16_len(&text));
                    let b = utf16_to_byte(&text, offset);
                    let g = text[b..].graphemes(true).next().unwrap_or(" ");
                    let tail = &text[b + g.len()..];
                    let marker = if self.focused { CURSOR_MARKER } else { "" };
                    text = format!("{}{}\x1b[7m{}\x1b[0m{}", &text[..b], marker, g, tail);
                }
                let fill = " ".repeat(content.saturating_sub(visible_width(&text)));
                rendered.push(Line::from_ansi(&format!(
                    "{}{}{}{}",
                    " ".repeat(padding),
                    text,
                    fill,
                    " ".repeat(padding)
                )));
            }
        }
        rendered.push(Line::plain("─".repeat(width)));
        if let Some(list) = &mut self.autocomplete {
            rendered.extend(list.render(content as u16).iter().cloned());
        }
        self.cached = rendered;
        &self.cached
    }
    fn invalidate(&mut self) {}
    fn handle_input(&mut self, data: &str) {
        Self::handle_input(self, data);
    }
    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}
impl Focusable for Editor<'_> {
    fn focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Tui;
    impl EditorTui for Tui {
        fn request_render(&self) {}
        fn terminal_rows(&self) -> u16 {
            24
        }
    }
    struct PathProvider;
    impl AutocompleteProvider for PathProvider {
        fn get_suggestions(
            &self,
            _lines: &[String],
            _line: usize,
            _col: usize,
            options: SuggestionOptions,
        ) -> Option<crate::autocomplete::AutocompleteSuggestions> {
            options
                .force
                .then(|| crate::autocomplete::AutocompleteSuggestions {
                    items: vec![AutocompleteItem {
                        value: "src/lib.rs".into(),
                        label: "src/lib.rs".into(),
                        description: None,
                    }],
                    prefix: String::new(),
                })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            _prefix: &str,
        ) -> crate::autocomplete::AppliedCompletion {
            let mut lines = lines.to_vec();
            let byte = utf16_to_byte(&lines[line], col);
            lines[line].insert_str(byte, &item.value);
            crate::autocomplete::AppliedCompletion {
                lines,
                cursor_line: line,
                cursor_col: col + utf16_len(&item.value),
            }
        }
        fn should_trigger_file_completion(
            &self,
            _lines: &[String],
            _line: usize,
            _col: usize,
        ) -> bool {
            true
        }
    }
    fn e() -> Editor<'static> {
        static T: Tui = Tui;
        Editor::new(&T, EditorTheme)
    }
    #[test]
    fn accessors_are_utf16() {
        let mut e = e();
        e.set_text("ä😀");
        assert_eq!(e.get_cursor(), (0, 3));
        assert_eq!(e.get_lines(), vec!["ä😀"]);
        e.handle_input("\x1b[D");
        assert_eq!(e.get_cursor(), (0, 1));
    }
    #[test]
    fn history_and_draft() {
        let mut e = e();
        e.add_to_history("one");
        e.add_to_history("two");
        e.handle_input("\x1b[A");
        assert_eq!(e.get_text(), "two");
        e.handle_input("\x1b[A");
        assert_eq!(e.get_text(), "one");
        e.handle_input("\x1b[B");
        assert_eq!(e.get_text(), "two");
        e.handle_input("\x1b[B");
        assert_eq!(e.get_text(), "");
    }
    #[test]
    fn backslash_enter_is_newline() {
        let mut e = e();
        e.handle_input("\\");
        e.handle_input("\r");
        assert_eq!(e.get_text(), "\n");
    }
    #[test]
    fn kitty_printable() {
        let mut e = e();
        e.handle_input("\x1b[69;2u");
        assert_eq!(e.get_text(), "E");
    }
    #[test]
    fn unicode_editing() {
        let mut e = e();
        e.handle_input("😀");
        e.handle_input("👍");
        e.handle_input("\x7f");
        assert_eq!(e.get_text(), "😀");
    }
    #[test]
    fn pasted_tabs_expand_to_four_spaces() {
        let mut e = e();
        e.insert_text_at_cursor("a\tb");
        assert_eq!(e.get_text(), "a    b");
    }
    #[test]
    fn wrapping_matches_pi_boundaries() {
        assert_eq!(
            word_wrap_line("hello world test", 11)
                .iter()
                .map(|x| x.text.as_str())
                .collect::<Vec<_>>(),
            vec!["hello ", "world test"]
        );
        assert_eq!(
            word_wrap_line("hello world test", 12)
                .iter()
                .map(|x| x.text.as_str())
                .collect::<Vec<_>>(),
            vec!["hello world ", "test"]
        );
    }
    #[test]
    fn kill_yank_and_undo() {
        let mut e = e();
        e.set_text("foo bar baz");
        e.handle_input("\x17");
        assert_eq!(e.get_text(), "foo bar ");
        e.handle_input("\x19");
        assert_eq!(e.get_text(), "foo bar baz");
        e.handle_input("\x1b[45;5u");
        assert_eq!(e.get_text(), "foo bar ");
    }
    #[test]
    fn paste_marker() {
        let mut e = e();
        e.handle_input("\x1b[200~");
        e.handle_input(&format!("{}\x1b[201~", "x\n".repeat(11)));
        assert_eq!(e.get_text(), "[paste #1 +12 lines]");
        assert_eq!(e.get_expanded_text(), "x\n".repeat(11));
    }
    #[test]
    fn character_jump() {
        let mut e = e();
        e.set_text("hello world");
        e.handle_input("\x01");
        e.handle_input("\x1d");
        e.handle_input("o");
        assert_eq!(e.get_cursor(), (0, 4));
    }
    #[test]
    fn undo_coalesces_words_but_not_spaces() {
        let mut e = e();
        for c in ["h", "e", "l", "l", "o", " ", "w", "o", "r", "l", "d"] {
            e.handle_input(c);
        }
        e.handle_input("\x1b[45;5u");
        assert_eq!(e.get_text(), "hello");
        e.handle_input("\x1b[45;5u");
        assert_eq!(e.get_text(), "");
    }
    #[test]
    fn tab_applies_single_path_completion() {
        let mut e = e();
        e.set_autocomplete_provider(Box::new(PathProvider));
        e.handle_input("\t");
        assert_eq!(e.get_text(), "src/lib.rs");
        assert!(!e.is_showing_autocomplete());
    }
    #[test]
    fn paste_decodes_csi_u_ctrl_j() {
        let mut e = e();
        e.handle_input("\x1b[200~line1\x1b[106;5uline2\x1b[201~");
        assert_eq!(e.get_text(), "line1\nline2");
    }
    #[test]
    fn paste_marker_delete_renumbers_remaining_markers() {
        let mut e = e();
        e.handle_input(&format!("\x1b[200~{}\x1b[201~", "x\n".repeat(11)));
        e.handle_input(" ");
        e.handle_input(&format!("\x1b[200~{}\x1b[201~", "y\n".repeat(11)));
        e.handle_input("\x01");
        e.handle_input("\x1b[C");
        e.handle_input("\x7f");
        assert_eq!(e.get_text(), " [paste #1 +12 lines]");
        assert_eq!(e.get_expanded_text(), format!(" {}", "y\n".repeat(11)));
    }
}
