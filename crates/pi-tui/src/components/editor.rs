//! Multiline editor — port of `packages/tui/src/components/editor.ts`.
//!
//! Cursor columns are JavaScript UTF-16 code-unit offsets.  All editing is
//! grapheme-aware even though storage is UTF-8.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

use crate::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CancellationToken,
    SuggestionOptions, SuggestionStart,
};
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
const ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS: u64 = 20;
const DEFAULT_TRIGGER_CHARS: &[char] = &['@', '#'];

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
            && let Some(end) = text[byte..].find(']')
        {
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
    word_wrap_line_inner(line, max_width, false, None)
}

/// Word-wrap treating paste-marker-shaped spans as atomic segments.
pub fn word_wrap_line_atomic(line: &str, max_width: usize) -> Vec<TextChunk> {
    word_wrap_line_inner(line, max_width, true, None)
}

/// Word-wrap with explicit pre-segmented atomic units (TS `preSegmented` arg).
///
/// Each segment is `(utf16_start, text)`. Oversized atomic segments are split
/// for layout only.
pub fn word_wrap_line_with_segments(
    line: &str,
    max_width: usize,
    segments: &[(usize, String)],
) -> Vec<TextChunk> {
    word_wrap_line_inner(line, max_width, true, Some(segments))
}

fn word_wrap_line_inner(
    line: &str,
    max_width: usize,
    atomic_markers: bool,
    pre_segmented: Option<&[(usize, String)]>,
) -> Vec<TextChunk> {
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
    let segs = if let Some(pre) = pre_segmented {
        pre.to_vec()
    } else if atomic_markers {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualLine {
    logical_line: usize,
    start_col: usize,
    length: usize,
}

struct PendingAutocomplete {
    force: bool,
    explicit_tab: bool,
    token: u64,
    ready_at: Instant,
    text: String,
    line: usize,
    col: usize,
    cancel: CancellationToken,
    /// In-flight async result, if provider returned Pending.
    rx: Option<Receiver<Option<AutocompleteSuggestions>>>,
    /// True once begin_suggestions has been invoked for this request.
    started: bool,
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
    snapped_from_col: Option<usize>,
    provider: Option<Box<dyn AutocompleteProvider>>,
    autocomplete: Option<SelectList>,
    autocomplete_prefix: String,
    autocomplete_force: bool,
    autocomplete_max_visible: usize,
    trigger_chars: Vec<char>,
    pending_ac: Option<PendingAutocomplete>,
    /// Cursor (line, col) where the last yank began (for yank-pop delete).
    last_yank_start: Option<(usize, usize)>,
    ac_token: u64,
    /// Debounce delay for attachment-style triggers (`@`, `#`, custom). Tests may set 0.
    ac_debounce_ms: u64,
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
            snapped_from_col: None,
            provider: None,
            autocomplete: None,
            autocomplete_prefix: String::new(),
            autocomplete_force: false,
            autocomplete_max_visible: {
                let n = if options.autocomplete_max_visible == 0 {
                    5
                } else {
                    options.autocomplete_max_visible
                };
                n.clamp(3, 20)
            },
            trigger_chars: DEFAULT_TRIGGER_CHARS.to_vec(),
            pending_ac: None,
            last_yank_start: None,
            ac_token: 0,
            ac_debounce_ms: ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS,
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
        self.cancel_autocomplete();
        self.last_action = None;
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
        let triggers = provider.trigger_characters().to_vec();
        self.provider = Some(provider);
        // Default @/# when provider lists none; otherwise provider triggers only.
        self.trigger_chars = if triggers.is_empty() {
            DEFAULT_TRIGGER_CHARS.to_vec()
        } else {
            triggers
        };
        self.cancel_autocomplete_request();
        self.cancel_autocomplete();
    }
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete.is_some()
    }
    /// Override attachment debounce (TS default 20ms). Use 0 in unit tests.
    pub fn set_autocomplete_debounce_ms(&mut self, ms: u64) {
        self.ac_debounce_ms = ms;
    }
    /// Process any pending debounced autocomplete request whose delay has elapsed.
    /// Also processes immediately-ready requests (delay 0).
    pub fn flush_autocomplete(&mut self) {
        self.poll_autocomplete(true);
    }
    /// UTF-16 code-unit length of `s` (JS string index helper for tests).
    pub fn utf16_len_of(s: &str) -> usize {
        utf16_len(s)
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
        self.snapped_from_col = None;
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
        self.marker_id(marker).map(|id| (utf16_len(marker), id))
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
                let Some(close) = line[start..].find(']') else {
                    break;
                };
                let end = start + close + 1;
                let marker = &line[start..end];
                if let Some(number) = marker
                    .strip_prefix(PASTE_PREFIX)
                    .and_then(|s| s.split_once(' '))
                    .and_then(|(n, _)| n.parse::<usize>().ok())
                    && number > id
                {
                    replacements.push((
                        start,
                        end,
                        marker.replacen(&format!("#{number}"), &format!("#{}", number - 1), 1),
                    ));
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
                self.preferred_col = None;
                self.snapped_from_col = None;
                self.changed();
            } else {
                self.set_text_inner("", false, false);
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
        // Autocomplete after insert (slash / trigger chars / continuation).
        if self.autocomplete.is_some() {
            self.update_autocomplete();
        } else if text == "/" && self.state.line == 0 {
            let before = self.line()[..utf16_to_byte(self.line(), self.state.col)].to_owned();
            if before.trim_start() == "/" || before.starts_with('/') {
                self.try_trigger_autocomplete(false);
            }
        } else if text.chars().count() == 1 {
            let ch = text.chars().next().unwrap();
            if self.trigger_chars.contains(&ch) {
                let before = self.line()[..utf16_to_byte(self.line(), self.state.col)].to_owned();
                let prev = before.chars().rev().nth(1);
                if before.chars().count() == 1 || prev == Some(' ') || prev == Some('\t') {
                    self.try_trigger_autocomplete(false);
                }
            } else {
                let before = self.line()[..utf16_to_byte(self.line(), self.state.col)].to_owned();
                if self.is_in_slash_context(&before) || self.matches_trigger_pattern(&before) {
                    self.try_trigger_autocomplete(false);
                }
            }
        }
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
                (
                    before
                        .graphemes(true)
                        .next_back()
                        .map(utf16_len)
                        .unwrap_or(1),
                    None,
                )
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
        if self.autocomplete.is_some() {
            self.update_autocomplete();
        } else {
            let before = self.line()[..utf16_to_byte(self.line(), self.state.col)].to_owned();
            if self.is_in_slash_context(&before) || self.matches_trigger_pattern(&before) {
                self.try_trigger_autocomplete(false);
            }
        }
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
                (
                    self.line()[b..]
                        .graphemes(true)
                        .next()
                        .map(utf16_len)
                        .unwrap_or(1),
                    None,
                )
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
        self.last_yank_start = Some((self.state.line, self.state.col));
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
        self.delete_yanked_text(&old);
        self.kill_ring.rotate();
        let replacement = self.kill_ring.peek().unwrap().to_owned();
        self.last_yank_start = Some((self.state.line, self.state.col));
        self.insert_text_inner(&replacement);
        self.last_action = Some(Action::Yank);
        self.changed();
    }
    /// Delete text previously inserted by yank, using the recorded start mark.
    fn delete_yanked_text(&mut self, text: &str) {
        let Some((start_line, start_col)) = self.last_yank_start else {
            // Fallback: assume cursor is immediately after a single-line yank.
            let start = self.state.col.saturating_sub(utf16_len(text));
            let b0 = utf16_to_byte(self.line(), start);
            let b1 = utf16_to_byte(self.line(), self.state.col);
            self.state.lines[self.state.line].replace_range(b0..b1, "");
            self.state.col = start;
            return;
        };
        let end_line = self.state.line;
        let end_col = self.state.col;
        if start_line == end_line {
            let b0 = utf16_to_byte(self.line(), start_col);
            let b1 = utf16_to_byte(self.line(), end_col);
            self.state.lines[self.state.line].replace_range(b0..b1, "");
            self.state.col = start_col;
            return;
        }
        // Multi-line: join prefix of start line with suffix of end line.
        let prefix = self.state.lines[start_line]
            [..utf16_to_byte(&self.state.lines[start_line], start_col)]
            .to_owned();
        let suffix = self.state.lines[end_line][utf16_to_byte(&self.state.lines[end_line], end_col)..]
            .to_owned();
        self.state
            .lines
            .splice(start_line..=end_line, [format!("{prefix}{suffix}")]);
        self.state.line = start_line;
        self.state.col = start_col;
        let _ = text; // used for single-line fallback only
    }

    fn undo(&mut self) {
        self.exit_history();
        if let Some(state) = self.undo.pop() {
            self.state = state;
            self.last_action = None;
            self.preferred_col = None;
            self.snapped_from_col = None;
            self.changed();
        }
    }
    fn move_horiz(&mut self, right: bool) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current_vl = self.find_current_visual_line(&visual_lines);
        if right {
            if let Some((len, _)) = self.atomic_at_cursor() {
                self.set_col(self.state.col + len);
            } else if self.state.col < utf16_len(self.line()) {
                let b = utf16_to_byte(self.line(), self.state.col);
                let step = self.line()[b..]
                    .graphemes(true)
                    .next()
                    .map(utf16_len)
                    .unwrap_or(1);
                self.set_col(self.state.col + step);
            } else if self.state.line + 1 < self.state.lines.len() {
                self.state.line += 1;
                self.set_col(0);
            } else if let Some(vl) = visual_lines.get(current_vl) {
                // At end of last line: keep preferred visual col for vertical nav.
                self.preferred_col = Some(self.state.col.saturating_sub(vl.start_col));
            }
        } else if let Some((start, _)) = self.atomic_before_cursor() {
            self.set_col(start);
        } else if self.state.col > 0 {
            let b = utf16_to_byte(self.line(), self.state.col);
            let step = self.line()[..b]
                .graphemes(true)
                .next_back()
                .map(utf16_len)
                .unwrap_or(1);
            self.set_col(self.state.col - step);
        } else if self.state.line > 0 {
            self.state.line -= 1;
            self.set_col(utf16_len(self.line()));
        }
        if self.autocomplete.is_some() {
            self.update_autocomplete();
        }
    }
    fn move_vert(&mut self, down: bool) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        if visual_lines.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&visual_lines);
        let target = if down {
            current + 1
        } else {
            current.saturating_sub(1)
        };
        if target >= visual_lines.len() || (!down && current == 0) {
            return;
        }
        self.move_to_visual_line(&visual_lines, current, target);
        if self.autocomplete.is_some() {
            self.update_autocomplete();
        }
    }
    fn page_scroll(&mut self, down: bool) {
        self.last_action = None;
        let rows = self.tui.terminal_rows() as usize;
        let page = (rows * 3 / 10).max(5);
        let visual_lines = self.build_visual_line_map(self.last_width);
        if visual_lines.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&visual_lines);
        let target = if down {
            (current + page).min(visual_lines.len() - 1)
        } else {
            current.saturating_sub(page)
        };
        self.move_to_visual_line(&visual_lines, current, target);
    }
    fn move_word(&mut self, forward: bool) {
        self.last_action = None;
        let is_atomic = |s: &str| -> bool { is_marker(s) && self.marker_id(s).is_some() };
        if forward {
            if self.state.col == utf16_len(self.line()) {
                self.move_horiz(true);
            } else {
                let col = find_word_forward(self.line(), self.state.col, Some(&is_atomic));
                self.set_col(col);
            }
        } else if self.state.col == 0 && self.state.line > 0 {
            self.state.line -= 1;
            self.set_col(utf16_len(self.line()));
        } else {
            let col = find_word_backward(self.line(), self.state.col, Some(&is_atomic));
            self.set_col(col);
        }
    }
    fn is_on_first_visual_line(&self) -> bool {
        let vls = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&vls) == 0
    }
    fn is_on_last_visual_line(&self) -> bool {
        let vls = self.build_visual_line_map(self.last_width);
        !vls.is_empty() && self.find_current_visual_line(&vls) + 1 == vls.len()
    }
    fn is_editor_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }
    fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let width = width.max(1);
        let mut out = Vec::new();
        for (i, line) in self.state.lines.iter().enumerate() {
            if line.is_empty() {
                out.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: 0,
                });
            } else if visible_width(line) <= width {
                out.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: utf16_len(line),
                });
            } else {
                for chunk in word_wrap_line_inner(line, width, true, None) {
                    out.push(VisualLine {
                        logical_line: i,
                        start_col: chunk.start_index,
                        length: chunk.end_index.saturating_sub(chunk.start_index),
                    });
                }
            }
        }
        out
    }
    fn find_visual_line_at(&self, visual_lines: &[VisualLine], line: usize, col: usize) -> usize {
        for (i, vl) in visual_lines.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col as isize - vl.start_col as isize;
            let is_last =
                i + 1 == visual_lines.len() || visual_lines[i + 1].logical_line != vl.logical_line;
            if offset >= 0
                && (offset < vl.length as isize || (is_last && offset == vl.length as isize))
            {
                return i;
            }
        }
        visual_lines.len().saturating_sub(1)
    }
    fn find_current_visual_line(&self, visual_lines: &[VisualLine]) -> usize {
        self.find_visual_line_at(visual_lines, self.state.line, self.state.col)
    }
    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max: usize,
        target_max: usize,
    ) -> usize {
        let has_preferred = self.preferred_col.is_some();
        let cursor_in_middle = current_visual_col < source_max;
        let target_too_short = target_max < current_visual_col;
        if !has_preferred || cursor_in_middle {
            if target_too_short {
                self.preferred_col = Some(current_visual_col);
                return target_max;
            }
            self.preferred_col = None;
            return current_visual_col;
        }
        let preferred = self.preferred_col.unwrap();
        let cant_fit = target_max < preferred;
        if target_too_short || cant_fit {
            return target_max;
        }
        self.preferred_col = None;
        preferred
    }
    fn move_to_visual_line(
        &mut self,
        visual_lines: &[VisualLine],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(current_vl) = visual_lines.get(current_visual_line).copied() else {
            return;
        };
        let Some(target_vl) = visual_lines.get(target_visual_line).copied() else {
            return;
        };
        let current_visual_col = if let Some(snap) = self.snapped_from_col {
            let vl_index = self.find_visual_line_at(visual_lines, current_vl.logical_line, snap);
            snap.saturating_sub(visual_lines[vl_index].start_col)
        } else {
            self.state.col.saturating_sub(current_vl.start_col)
        };
        let is_last_source = current_visual_line + 1 == visual_lines.len()
            || visual_lines[current_visual_line + 1].logical_line != current_vl.logical_line;
        let source_max = if is_last_source {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };
        let is_last_target = target_visual_line + 1 == visual_lines.len()
            || visual_lines[target_visual_line + 1].logical_line != target_vl.logical_line;
        let target_max = if is_last_target {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
        };
        let move_to = self.compute_vertical_move_column(current_visual_col, source_max, target_max);
        self.state.line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to;
        let logical = self.state.lines[target_vl.logical_line].clone();
        self.state.col = target_col.min(utf16_len(&logical));

        // Snap into atomic multi-grapheme segments (paste markers).
        let segs = graphemes_with_markers(&logical);
        for (index, segment) in segs {
            let seg_len = utf16_len(&segment);
            if index > self.state.col {
                break;
            }
            if seg_len <= 1 {
                continue;
            }
            if self.state.col < index + seg_len {
                let is_continuation = index < target_vl.start_col;
                let is_moving_down = target_visual_line > current_visual_line;
                if is_continuation && is_moving_down {
                    let seg_end = index + seg_len;
                    let mut next = target_visual_line + 1;
                    while next < visual_lines.len()
                        && visual_lines[next].logical_line == target_vl.logical_line
                        && visual_lines[next].start_col < seg_end
                    {
                        next += 1;
                    }
                    if next < visual_lines.len() {
                        self.move_to_visual_line(visual_lines, current_visual_line, next);
                        return;
                    }
                }
                self.snapped_from_col = Some(self.state.col);
                self.state.col = index;
                return;
            }
        }
        self.snapped_from_col = None;
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
                && ((65..=90).contains(&cp) || (97..=122).contains(&cp))
            {
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
        self.request_autocomplete(force, force);
    }
    fn try_trigger_autocomplete(&mut self, explicit_tab: bool) {
        self.request_autocomplete(false, explicit_tab);
    }
    fn update_autocomplete(&mut self) {
        if self.autocomplete.is_none() {
            return;
        }
        let force = self.autocomplete_force;
        self.request_autocomplete(force, false);
    }
    fn cancel_autocomplete_request(&mut self) {
        if let Some(pending) = self.pending_ac.take() {
            // Push-based abort: listeners fire even though we drop the job.
            pending.cancel.cancel();
        }
        // Bump token so a late channel delivery is ignored.
        self.ac_token = self.ac_token.wrapping_add(1);
    }
    fn debounce_ms_for(&self, force: bool, explicit_tab: bool) -> u64 {
        if force || explicit_tab {
            return 0;
        }
        let before = self.line()[..utf16_to_byte(self.line(), self.state.col)].to_owned();
        if self.matches_debounce_pattern(&before) {
            self.ac_debounce_ms
        } else {
            0
        }
    }
    fn matches_trigger_pattern(&self, before: &str) -> bool {
        // (?:^|[\s])[triggers][^\s]*$
        if before.is_empty() {
            return false;
        }
        let chars: Vec<char> = before.chars().collect();
        let mut i = chars.len();
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        // i is start of last token
        if i >= chars.len() {
            return false;
        }
        let first = chars[i];
        if !self.trigger_chars.contains(&first) {
            return false;
        }
        if i > 0 && !chars[i - 1].is_whitespace() {
            return false;
        }
        true
    }
    fn matches_debounce_pattern(&self, before: &str) -> bool {
        // (?:^|[ \t])(?:@"..."|@\S*|[other][^\s]*)$
        if before.is_empty() {
            return false;
        }
        let chars: Vec<char> = before.chars().collect();
        // find last space/tab or start
        let mut i = chars.len();
        while i > 0 && chars[i - 1] != ' ' && chars[i - 1] != '\t' {
            i -= 1;
        }
        if i >= chars.len() {
            return false;
        }
        let token: String = chars[i..].iter().collect();
        if let Some(token) = token.strip_prefix('@') {
            // @"..." or @nonspace*
            if let Some(quoted) = token.strip_prefix('"') {
                return !quoted.contains('"');
            }
            return !token.chars().any(|c| c.is_whitespace());
        }
        let others: Vec<char> = self
            .trigger_chars
            .iter()
            .copied()
            .filter(|c| *c != '@')
            .collect();
        if others.is_empty() {
            return false;
        }
        let first = token.chars().next().unwrap();
        others.contains(&first) && !token.chars().skip(1).any(|c| c.is_whitespace())
    }
    fn is_in_slash_context(&self, before: &str) -> bool {
        let trimmed = before.trim_start();
        trimmed.starts_with('/') && self.state.line == 0
    }
    fn request_autocomplete(&mut self, force: bool, explicit_tab: bool) {
        if self.provider.is_none() {
            return;
        }
        if force {
            let ok = self
                .provider
                .as_ref()
                .map(|p| {
                    p.should_trigger_file_completion(
                        &self.state.lines,
                        self.state.line,
                        self.state.col,
                    )
                })
                .unwrap_or(false);
            if !ok {
                return;
            }
        }
        self.cancel_autocomplete_request();
        self.ac_token = self.ac_token.wrapping_add(1);
        let token = self.ac_token;
        let delay = self.debounce_ms_for(force, explicit_tab);
        self.pending_ac = Some(PendingAutocomplete {
            force,
            explicit_tab,
            token,
            ready_at: Instant::now() + Duration::from_millis(delay),
            text: self.get_text(),
            line: self.state.line,
            col: self.state.col,
            cancel: CancellationToken::new(),
            rx: None,
            started: false,
        });
        if delay == 0 {
            self.poll_autocomplete(true);
        }
    }
    #[allow(clippy::collapsible_if)]
    fn poll_autocomplete(&mut self, force_ready: bool) {
        // 1) Drain in-flight async result if present.
        if let Some(pending) = self.pending_ac.as_mut() {
            if pending.started {
                if pending.cancel.is_cancelled() || pending.token != self.ac_token {
                    self.pending_ac = None;
                    return;
                }
                if let Some(rx) = pending.rx.as_ref() {
                    match rx.try_recv() {
                        Ok(result) => {
                            let pending = self.pending_ac.take().unwrap();
                            if pending.cancel.is_cancelled()
                                || pending.token != self.ac_token
                                || pending.text != self.get_text()
                                || pending.line != self.state.line
                                || pending.col != self.state.col
                            {
                                return;
                            }
                            self.apply_suggestion_result(result, pending.force, pending.explicit_tab);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            // Still waiting; nothing else to do.
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            self.pending_ac = None;
                        }
                    }
                    return;
                }
            }
        }

        let Some(pending) = self.pending_ac.as_ref() else {
            return;
        };
        if pending.started {
            return;
        }
        if !force_ready && Instant::now() < pending.ready_at {
            return;
        }
        if pending.cancel.is_cancelled() {
            self.pending_ac = None;
            return;
        }
        // Snapshot fields we need after take.
        let force = pending.force;
        let explicit_tab = pending.explicit_tab;
        let token = pending.token;
        let text = pending.text.clone();
        let line = pending.line;
        let col = pending.col;
        let cancel = pending.cancel.clone();

        // Stale if document moved since schedule.
        if text != self.get_text()
            || line != self.state.line
            || col != self.state.col
            || token != self.ac_token
        {
            self.pending_ac = None;
            return;
        }
        if cancel.is_cancelled() {
            self.pending_ac = None;
            return;
        }
        let Some(provider) = self.provider.as_ref() else {
            self.pending_ac = None;
            return;
        };
        let start = provider.begin_suggestions(
            &self.state.lines,
            self.state.line,
            self.state.col,
            SuggestionOptions {
                force,
                cancel: cancel.clone(),
            },
        );
        match start {
            SuggestionStart::Ready(result) => {
                self.pending_ac = None;
                if cancel.is_cancelled() || token != self.ac_token {
                    return;
                }
                self.apply_suggestion_result(result, force, explicit_tab);
            }
            SuggestionStart::Pending(rx) => {
                if let Some(p) = self.pending_ac.as_mut() {
                    p.started = true;
                    p.rx = Some(rx);
                }
                // Immediate non-blocking poll in case the result is already ready.
                self.poll_autocomplete(true);
            }
        }
    }

    fn apply_suggestion_result(
        &mut self,
        result: Option<AutocompleteSuggestions>,
        force: bool,
        explicit_tab: bool,
    ) {
        let Some(s) = result else {
            self.cancel_autocomplete();
            self.tui.request_render();
            return;
        };
        if s.items.is_empty() {
            self.cancel_autocomplete();
            self.tui.request_render();
            return;
        }
        if force && explicit_tab && s.items.len() == 1 {
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
        let mut list = SelectList::new(
            items,
            self.autocomplete_max_visible,
            SelectListTheme::identity(),
            Default::default(),
        );
        // Exact match wins; else first prefix match.
        let mut first_prefix = None;
        let mut exact = None;
        for (i, item) in s.items.iter().enumerate() {
            if item.value == s.prefix {
                exact = Some(i);
                break;
            }
            if first_prefix.is_none() && item.value.starts_with(&s.prefix) {
                first_prefix = Some(i);
            }
        }
        if let Some(n) = exact.or(first_prefix) {
            list.set_selected_index(n);
        }
        self.autocomplete = Some(list);
        self.autocomplete_prefix = s.prefix;
        self.autocomplete_force = force;
        self.tui.request_render();
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
        self.last_action = None;
        self.state.lines = applied.lines;
        self.state.line = applied.cursor_line;
        self.set_col(applied.cursor_col);
        self.cancel_autocomplete_request();
        self.cancel_autocomplete();
        self.changed();
    }
    fn cancel_autocomplete(&mut self) {
        self.cancel_autocomplete_request();
        self.autocomplete = None;
        self.autocomplete_prefix.clear();
        self.autocomplete_force = false;
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
        } else if kb.matches(data, "tui.editor.pageUp") {
            self.page_scroll(false);
        } else if kb.matches(data, "tui.editor.pageDown") {
            self.page_scroll(true);
        } else if kb.matches(data, "tui.editor.cursorUp") {
            if self.is_on_first_visual_line()
                && (self.is_editor_empty() || self.history_index > -1 || self.state.col == 0)
            {
                self.navigate_history(true);
            } else if self.is_on_first_visual_line() {
                self.last_action = None;
                self.set_col(0);
            } else {
                self.move_vert(false);
            }
        } else if kb.matches(data, "tui.editor.cursorDown") {
            if self.history_index > -1 && self.is_on_last_visual_line() {
                self.navigate_history(false);
            } else if self.is_on_last_visual_line() {
                self.last_action = None;
                self.set_col(utf16_len(self.line()));
            } else {
                self.move_vert(true);
            }
        } else if self.autocomplete.is_some() && kb.matches(data, "tui.select.cancel") {
            self.cancel_autocomplete_request();
            self.cancel_autocomplete();
            self.tui.request_render();
        } else if self.autocomplete.is_some()
            && (kb.matches(data, "tui.select.up") || kb.matches(data, "tui.select.down"))
        {
            if let Some(list) = self.autocomplete.as_mut() {
                list.handle_input(data);
            }
            self.tui.request_render();
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
        } else if self.autocomplete.is_some() && kb.matches(data, "tui.select.confirm") {
            // Enter while menu open: apply selection. Slash prefixes fall through to submit.
            let is_slash = self.autocomplete_prefix.starts_with('/');
            if let Some(list) = &self.autocomplete
                && let Some(selected) = list.get_selected_item()
            {
                    let item = AutocompleteItem {
                        value: selected.value.clone(),
                        label: selected.label.clone(),
                        description: selected.description.clone(),
                    };
                    let prefix = self.autocomplete_prefix.clone();
                    self.apply_completion(&item, &prefix);
                    if is_slash {
                        self.submit();
                    }
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
        self.poll_autocomplete(false);
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
            for part in word_wrap_line_inner(logical, layout, true, None) {
                let has = line_index == self.state.line
                    && self.state.col >= part.start_index
                    && self.state.col <= part.end_index;
                let mut text = part.text;
                if has {
                    let offset = (self.state.col - part.start_index).min(utf16_len(&text));
                    let b = utf16_to_byte(&text, offset);
                    let (g, tail) = match text[b..].graphemes(true).next() {
                        Some(g) => (g, &text[b + g.len()..]),
                        None => (" ", ""),
                    };
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

    #[test]
    fn visual_line_map_uses_utf16_offsets_and_wraps() {
        let mut e = e();
        e.set_text("ab😀cdef");
        let visual = e.build_visual_line_map(4);
        assert_eq!(
            visual,
            vec![
                VisualLine {
                    logical_line: 0,
                    start_col: 0,
                    length: 4
                },
                VisualLine {
                    logical_line: 0,
                    start_col: 4,
                    length: 4
                },
            ]
        );
    }

    #[test]
    fn moves_through_wrapped_visual_lines_before_logical_lines() {
        let mut e = e();
        e.set_text("short\n123456789012345678901234567890");
        e.render(15);
        assert_eq!(e.get_cursor(), (1, 30));
        e.handle_input("\x1b[A");
        assert_eq!(e.get_cursor().0, 1);
        e.handle_input("\x1b[A");
        assert_eq!(e.get_cursor().0, 1);
        e.handle_input("\x1b[A");
        assert_eq!(e.get_cursor().0, 0);
    }

    #[test]
    fn sticky_column_restores_across_short_visual_line() {
        let mut e = e();
        e.set_text("2222222222x222\n\n1111111111_111111111111");
        e.handle_input("\x01");
        for _ in 0..10 {
            e.handle_input("\x1b[C");
        }
        e.handle_input("\x1b[A");
        assert_eq!(e.get_cursor(), (1, 0));
        e.handle_input("\x1b[A");
        assert_eq!(e.get_cursor(), (0, 10));
    }

    #[test]
    fn horizontal_movement_resets_sticky_column() {
        let mut e = e();
        e.set_text("1234567890\n\n1234567890");
        e.handle_input("\x01");
        for _ in 0..5 {
            e.handle_input("\x1b[C");
        }
        e.handle_input("\x1b[A");
        e.handle_input("\x1b[A");
        e.handle_input("\x1b[D");
        e.handle_input("\x1b[B");
        e.handle_input("\x1b[B");
        assert_eq!(e.get_cursor(), (2, 4));
    }

    #[test]
    fn page_scroll_moves_by_terminal_page_of_visual_lines() {
        let mut e = e();
        e.set_text(
            &(0..20)
                .map(|n| format!("line{n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        for _ in 0..19 {
            e.handle_input("\x1b[A");
        }
        e.handle_input("\x01");
        e.handle_input("\x1b[6~");
        assert_eq!(e.get_cursor(), (7, 0));
        e.handle_input("\x1b[5~");
        assert_eq!(e.get_cursor(), (0, 0));
    }

    #[test]
    fn resize_rewraps_visual_navigation_using_current_width() {
        let mut e = e();
        e.set_text("abcdefghijklmnopqr\n123456789012345678");
        e.handle_input("\x1b[A");
        e.handle_input("\x01");
        for _ in 0..18 {
            e.handle_input("\x1b[C");
        }
        e.render(10);
        e.handle_input("\x1b[B");
        assert_eq!(e.get_cursor(), (1, 8));
        e.render(80);
        e.handle_input("\x1b[A");
        assert_eq!(e.get_cursor(), (0, 8));
    }

    #[test]
    fn attachment_autocomplete_is_deferred_and_latest_request_wins() {
        struct Provider;
        impl AutocompleteProvider for Provider {
            fn get_suggestions(
                &self,
                lines: &[String],
                _: usize,
                col: usize,
                _: SuggestionOptions,
            ) -> Option<crate::autocomplete::AutocompleteSuggestions> {
                Some(crate::autocomplete::AutocompleteSuggestions {
                    items: vec![AutocompleteItem {
                        value: "@main.rs".into(),
                        label: "@main.rs".into(),
                        description: None,
                    }],
                    prefix: lines[0][..utf16_to_byte(&lines[0], col)].to_owned(),
                })
            }
            fn apply_completion(
                &self,
                lines: &[String],
                line: usize,
                col: usize,
                item: &AutocompleteItem,
                _: &str,
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
        }
        let mut e = e();
        e.set_autocomplete_provider(Box::new(Provider));
        e.handle_input("@");
        e.handle_input("m");
        assert!(!e.is_showing_autocomplete());
        e.flush_autocomplete();
        assert!(e.is_showing_autocomplete());
    }
}
