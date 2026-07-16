//! Oracle-faithful editor tests ported from
//! `.references/pi/packages/tui/test/editor.test.ts` (180 cases).
//!
//! Expected values are copied from the TypeScript asserts, never from Rust
//! output. Mapping table lives in `.outline/sdd/task-p2-report.md`.
#![allow(dead_code, unused_imports)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pi_tui::autocomplete::{
    AppliedCompletion, AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions,
    CancellationToken, CombinedAutocompleteProvider, CommandEntry, SlashCommand, SuggestionOptions,
    SuggestionStart,
};
use pi_tui::component::Component;
use pi_tui::components::editor::{
    Editor, EditorOptions, EditorTheme, EditorTui, word_wrap_line, word_wrap_line_with_segments,
};
use pi_tui::components::input::{utf16_len, utf16_to_byte};
use pi_tui::line::CURSOR_MARKER;
use pi_tui::util::visible_width;

struct TestTui {
    rows: u16,
    cols: u16,
}

impl EditorTui for TestTui {
    fn request_render(&self) {}
    fn terminal_rows(&self) -> u16 {
        self.rows
    }
}

fn tui() -> TestTui {
    TestTui { rows: 24, cols: 80 }
}

fn tui_size(cols: u16, rows: u16) -> TestTui {
    TestTui { rows, cols }
}

fn editor<'a>(tui: &'a TestTui) -> Editor<'a> {
    Editor::new(tui, EditorTheme)
}

fn editor_opts<'a>(tui: &'a TestTui, padding_x: usize) -> Editor<'a> {
    Editor::with_options(
        tui,
        EditorTheme,
        EditorOptions {
            padding_x,
            autocomplete_max_visible: 0,
        },
    )
}

/// Standard applyCompletion: replace prefix with item.value (TS test helper).
fn apply_completion(
    lines: &[String],
    cursor_line: usize,
    cursor_col: usize,
    item: &AutocompleteItem,
    prefix: &str,
) -> AppliedCompletion {
    let line = lines.get(cursor_line).cloned().unwrap_or_default();
    let col = cursor_col.min(utf16_len(&line));
    let prefix_len = utf16_len(prefix).min(col);
    let before_b = utf16_to_byte(&line, col - prefix_len);
    let after_b = utf16_to_byte(&line, col);
    let new_line = format!("{}{}{}", &line[..before_b], item.value, &line[after_b..]);
    let new_col = col - prefix_len + utf16_len(&item.value);
    let mut new_lines = lines.to_vec();
    if cursor_line < new_lines.len() {
        new_lines[cursor_line] = new_line;
    } else {
        new_lines.push(new_line);
    }
    AppliedCompletion {
        lines: new_lines,
        cursor_line,
        cursor_col: new_col,
    }
}

fn strip_cursor(s: &str) -> String {
    s.replace(CURSOR_MARKER, "")
}

fn render_plain(editor: &mut Editor<'_>, width: u16) -> Vec<String> {
    editor
        .render(width)
        .iter()
        .map(|l| strip_cursor(&l.plain_text()))
        .collect()
}

fn content_lines(rendered: &[String]) -> Vec<String> {
    // Editor renders top border, content…, bottom border.
    if rendered.len() <= 2 {
        return vec![];
    }
    rendered[1..rendered.len() - 1].to_vec()
}

fn position_cursor(editor: &mut Editor<'_>, line: usize, col: usize) {
    for _ in 0..20 {
        editor.handle_input("\x1b[A");
    }
    for _ in 0..line {
        editor.handle_input("\x1b[B");
    }
    editor.handle_input("\x01");
    for _ in 0..col {
        editor.handle_input("\x1b[C");
    }
}

fn paste_with_marker(editor: &mut Editor<'_>) -> String {
    let big = "line\n".repeat(20);
    let big = big.trim_end();
    editor.handle_input(&format!("\x1b[200~{big}\x1b[201~"));
    editor.get_text()
}

// ---------- Prompt history navigation (oracle 1-15) ----------

#[test]
fn history_does_nothing_on_up_when_empty() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "");
}

#[test]
fn history_shows_most_recent_on_up_when_empty() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("first prompt");
    e.add_to_history("second prompt");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "second prompt");
}

#[test]
fn history_cycles_on_repeated_up() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("third");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "third");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "second");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "first");
}

#[test]
fn history_jumps_to_start_before_entering_from_draft() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("prompt");
    e.set_text("draft");
    e.handle_input("[D");
    e.handle_input("[D");
    e.handle_input("[A"); // jumps to start before history
    assert_eq!(e.get_text(), "draft");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("[A"); // shows prompt
    assert_eq!(e.get_text(), "prompt");
    e.handle_input("[B"); // restores draft
    assert_eq!(e.get_text(), "draft");
    assert_eq!(e.get_cursor(), (0, 0));
}

#[test]
fn history_navigates_forward_with_down() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("third");
    e.set_text("draft");
    e.handle_input("[A"); // start of draft
    e.handle_input("[A"); // third
    e.handle_input("[A"); // second
    e.handle_input("[A"); // first
    e.handle_input("[B");
    assert_eq!(e.get_text(), "second");
    e.handle_input("[B");
    assert_eq!(e.get_text(), "third");
    e.handle_input("[B");
    assert_eq!(e.get_text(), "draft");
}

#[test]
fn history_exits_when_typing_character() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("old prompt");
    e.handle_input("\x1b[A");
    e.handle_input("x");
    assert_eq!(e.get_text(), "xold prompt");
}

#[test]
fn history_exits_on_set_text() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("first");
    e.add_to_history("second");
    e.handle_input("[A");
    e.set_text("");
    e.handle_input("[A");
    assert_eq!(e.get_text(), "second");
}

#[test]
fn history_does_not_add_empty_strings() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("");
    e.add_to_history("   ");
    e.add_to_history("valid");
    e.handle_input("[A");
    assert_eq!(e.get_text(), "valid");
    e.handle_input("[A");
    assert_eq!(e.get_text(), "valid");
}

#[test]
fn history_does_not_add_consecutive_duplicates() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("same");
    e.add_to_history("same");
    e.add_to_history("same");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "same");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "same");
}

#[test]
fn history_allows_non_consecutive_duplicates() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("first");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "first");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "second");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "first");
}

#[test]
fn history_uses_cursor_movement_when_editor_has_content() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("history item");
    e.set_text("line1\nline2");
    e.handle_input("\x1b[A"); // up within content
    e.handle_input("X");
    assert_eq!(e.get_text(), "line1X\nline2");
}

#[test]
fn history_limits_to_100_entries() {
    let t = tui();
    let mut e = editor(&t);
    for i in 0..105 {
        e.add_to_history(&format!("prompt {i}"));
    }
    for _ in 0..100 {
        e.handle_input("[A");
    }
    assert_eq!(e.get_text(), "prompt 5");
    e.handle_input("[A");
    assert_eq!(e.get_text(), "prompt 5");
}

#[test]
fn history_cursor_at_start_after_browsing_upward() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("older entry");
    e.add_to_history("line1\nline2\nline3");
    e.handle_input("[A");
    assert_eq!(e.get_text(), "line1\nline2\nline3");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("[A");
    assert_eq!(e.get_text(), "older entry");
    assert_eq!(e.get_cursor(), (0, 0));
}

#[test]
fn history_cursor_at_end_after_browsing_downward() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("older entry");
    e.add_to_history("line1\nline2\nline3");
    e.add_to_history("newer entry");
    e.handle_input("[A"); // newer
    e.handle_input("[A"); // multi
    e.handle_input("[A"); // older
    e.handle_input("[B"); // multi at end
    assert_eq!(e.get_text(), "line1\nline2\nline3");
    assert_eq!(e.get_cursor(), (2, 5));
    e.handle_input("[B"); // newer
    assert_eq!(e.get_text(), "newer entry");
}

#[test]
fn history_allows_opposite_direction_cursor_within_multiline() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("line1\nline2\nline3");
    e.handle_input("[A");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("[B");
    assert_eq!(e.get_text(), "line1\nline2\nline3");
    assert_eq!(e.get_cursor(), (1, 0));
    e.handle_input("[A");
    assert_eq!(e.get_text(), "line1\nline2\nline3");
    assert_eq!(e.get_cursor(), (0, 0));
}
