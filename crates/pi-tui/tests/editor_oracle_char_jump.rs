//! Oracle editor tests — Character jump (Ctrl+])
#![allow(dead_code, unused_imports)]

use pi_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, AppliedCompletion,
    CancellationToken, CombinedAutocompleteProvider, CommandEntry, SlashCommand,
    SuggestionOptions, SuggestionStart,
};
use pi_tui::component::Component;
use pi_tui::components::editor::{
    Editor, EditorOptions, EditorTheme, EditorTui, word_wrap_line, word_wrap_line_with_segments,
};
use pi_tui::components::input::{utf16_len, utf16_to_byte};
use pi_tui::line::CURSOR_MARKER;
use pi_tui::util::visible_width;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
fn editor<'a>(t: &'a TestTui) -> Editor<'a> {
    Editor::new(t, EditorTheme)
}
fn editor_opts<'a>(t: &'a TestTui, padding_x: usize) -> Editor<'a> {
    Editor::with_options(
        t,
        EditorTheme,
        EditorOptions {
            padding_x,
            autocomplete_max_visible: 0,
        },
    )
}
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

#[test]
fn jumps_forward_to_first_occurrence_of_character_on_same_line() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("o");
    assert_eq!(e.get_cursor(), (0, 4));
}

#[test]
fn jumps_forward_to_next_occurrence_after_cursor() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..4 { e.handle_input("\x1b[C"); }
    assert_eq!(e.get_cursor(), (0, 4));
    e.handle_input("\x1d");
    e.handle_input("o");
    assert_eq!(e.get_cursor(), (0, 7));
}

#[test]
fn jumps_forward_across_multiple_lines() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("abc\ndef\nghi");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("g");
    assert_eq!(e.get_cursor(), (2, 0));
}

#[test]
fn jumps_backward_to_first_occurrence_before_cursor_on_same_line() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    assert_eq!(e.get_cursor(), (0, 11));
    e.handle_input("\x1b\x1d");
    e.handle_input("o");
    assert_eq!(e.get_cursor(), (0, 7));
}

#[test]
fn jumps_backward_across_multiple_lines() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("abc\ndef\nghi");
    assert_eq!(e.get_cursor(), (2, 3));
    e.handle_input("\x1b\x1d");
    e.handle_input("a");
    assert_eq!(e.get_cursor(), (0, 0));
}

#[test]
fn is_case_sensitive() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("Hello World");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("h");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("W");
    assert_eq!(e.get_cursor(), (0, 6));
}

#[test]
fn cancels_jump_mode_when_ctrl_is_pressed_again() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("\x1d");
    e.handle_input("o");
    assert_eq!(e.get_text(), "ohello world");
}

#[test]
fn cancels_backward_jump_mode_when_ctrl_alt_is_pressed_again() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    assert_eq!(e.get_cursor(), (0, 11));
    e.handle_input("\x1b\x1d");
    e.handle_input("\x1b\x1d");
    e.handle_input("o");
    assert_eq!(e.get_text(), "hello worldo");
}

#[test]
fn searches_for_special_characters() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("foo(bar) = baz;");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("(");
    assert_eq!(e.get_cursor(), (0, 3));
    e.handle_input("\x1d");
    e.handle_input("=");
    assert_eq!(e.get_cursor(), (0, 9));
}

#[test]
fn handles_empty_text_gracefully() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("x");
    assert_eq!(e.get_cursor(), (0, 0));
}

#[test]
fn resets_lastaction_when_jumping() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    e.handle_input("x");
    assert_eq!(e.get_text(), "xhello world");
    e.handle_input("\x1d");
    e.handle_input("o");
    e.handle_input("Y");
    assert_eq!(e.get_text(), "xhellYo world");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "xhello world");
}

#[test]
fn does_nothing_when_character_is_not_found_forward() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("z");
    assert_eq!(e.get_cursor(), (0, 0));
}

#[test]
fn does_nothing_when_character_is_not_found_backward() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    assert_eq!(e.get_cursor(), (0, 11));
    e.handle_input("\x1b\x1d");
    e.handle_input("z");
    assert_eq!(e.get_cursor(), (0, 11));
}

#[test]
fn cancels_jump_mode_on_escape_and_processes_the_escape() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1d");
    e.handle_input("\x1b");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("o");
    assert_eq!(e.get_text(), "ohello world");
}
