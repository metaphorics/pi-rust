//! Oracle editor tests — Sticky column
#![allow(dead_code, unused_imports)]

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
fn preserves_target_column_when_moving_up_through_a_shorter_line() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("2222222222x222\n\n1111111111_111111111111");
    assert_eq!(e.get_cursor(), (2, 23));
    e.handle_input("\x01");
    for _ in 0..10 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (2, 10));
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (1, 0));
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 10));
}

#[test]
fn preserves_target_column_when_moving_down_through_a_shorter_line() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1111111111_111\n\n2222222222x222222222222");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x01");
    for _ in 0..10 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (0, 10));
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (1, 0));
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 10));
}

#[test]
fn resets_sticky_column_on_horizontal_movement_left_arrow() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\n\n1234567890");
    e.handle_input("\x01");
    for _ in 0..5 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (2, 5));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 5));
    e.handle_input("\x1b[D");
    assert_eq!(e.get_cursor(), (0, 4));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 4));
}

#[test]
fn resets_sticky_column_on_horizontal_movement_right_arrow() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\n\n1234567890");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x01");
    for _ in 0..5 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (0, 5));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 5));
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (2, 6));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 6));
}

#[test]
fn resets_sticky_column_on_typing() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\n\n1234567890");
    e.handle_input("\x01");
    for _ in 0..8 {
        e.handle_input("\x1b[C");
    }
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 8));
    e.handle_input("X");
    assert_eq!(e.get_cursor(), (0, 9));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 9));
}

#[test]
fn resets_sticky_column_on_backspace() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\n\n1234567890");
    e.handle_input("\x01");
    for _ in 0..8 {
        e.handle_input("\x1b[C");
    }
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 8));
    e.handle_input("\x7f");
    assert_eq!(e.get_cursor(), (0, 7));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 7));
}

#[test]
fn resets_sticky_column_on_ctrl_a_move_to_line_start() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\n\n1234567890");
    e.handle_input("\x01");
    for _ in 0..8 {
        e.handle_input("\x1b[C");
    }
    e.handle_input("\x1b[A");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (1, 0));
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 0));
}

#[test]
fn resets_sticky_column_on_ctrl_e_move_to_line_end() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("12345\n\n1234567890");
    e.handle_input("\x01");
    for _ in 0..3 {
        e.handle_input("\x1b[C");
    }
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 3));
    e.handle_input("\x05");
    assert_eq!(e.get_cursor(), (0, 5));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 5));
}

#[test]
fn resets_sticky_column_on_word_movement_ctrl_left() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world\n\nhello world");
    assert_eq!(e.get_cursor(), (2, 11));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 11));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 6));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 6));
}

#[test]
fn resets_sticky_column_on_word_movement_ctrl_right() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world\n\nhello world");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x01");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 0));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (2, 5));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 5));
}

#[test]
fn resets_sticky_column_on_undo() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\n\n1234567890");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x01");
    for _ in 0..8 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (0, 8));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 8));
    e.handle_input("X");
    assert_eq!(e.get_text(), "1234567890\n\n12345678X90");
    assert_eq!(e.get_cursor(), (2, 9));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 9));
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "1234567890\n\n1234567890");
    assert_eq!(e.get_cursor(), (2, 8));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 8));
}

#[test]
fn handles_multiple_consecutive_up_down_movements() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\nab\ncd\nef\n1234567890");
    e.handle_input("\x01");
    for _ in 0..7 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (4, 7));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 7));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (4, 7));
}

#[test]
fn moves_correctly_through_wrapped_visual_lines_without_getting_stuck() {
    let t = tui_size(15, 24);
    let mut e = editor(&t);
    e.set_text("short\n123456789012345678901234567890");
    let _ = e.render(15);
    assert_eq!(e.get_cursor(), (1, 30));
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor().0, 1);
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor().0, 1);
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor().0, 0);
}

#[test]
fn handles_settext_resetting_sticky_column() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890\n\n1234567890");
    e.handle_input("\x01");
    for _ in 0..8 {
        e.handle_input("\x1b[C");
    }
    e.handle_input("\x1b[A");
    e.set_text("abcdefghij\n\nabcdefghij");
    assert_eq!(e.get_cursor(), (2, 10));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 10));
}

#[test]
fn handles_editor_resizes_when_preferredvisualcol_is_on_the_same_line() {
    let t = tui_size(80, 24);
    let mut e = editor(&t);
    e.set_text("12345678901234567890\n\n12345678901234567890");
    e.handle_input("\x01");
    for _ in 0..15 {
        e.handle_input("\x1b[C");
    }
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 15));
    let _ = e.render(12);
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor().1, 4);
}

#[test]
fn handles_editor_resizes_when_preferredvisualcol_is_on_a_different_line() {
    let t = tui_size(80, 24);
    let mut e = editor(&t);
    e.set_text("short\n12345678901234567890");
    e.handle_input("\x01");
    for _ in 0..15 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (1, 15));
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 5));
    let _ = e.render(10);
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (1, 8));
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 5));
    let _ = e.render(80);
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (1, 15));
}

#[test]
fn rewrapped_lines_target_fits_current_visual_column() {
    let t = tui_size(80, 24);
    let mut e = editor(&t);
    e.set_text("abcdefghijklmnopqr\n123456789012345678");
    position_cursor(&mut e, 0, 18);
    assert_eq!(e.get_cursor(), (0, 18));
    let _ = e.render(10);
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (1, 8));
    let _ = e.render(80);
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 8));
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (1, 8));
}

#[test]
fn rewrapped_lines_target_shorter_than_current_visual_column() {
    let t = tui_size(80, 24);
    let mut e = editor(&t);
    e.set_text("abcdefghijklmnopqr\n123456789012345678\nab");
    position_cursor(&mut e, 0, 18);
    assert_eq!(e.get_cursor(), (0, 18));
    let _ = e.render(10);
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (1, 8));
    let _ = e.render(80);
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 2));
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (1, 8));
}

#[test]
fn sets_preferred_visual_col_when_pressing_right_at_end_of_prompt() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("111111111x1111111111\n\n333333333_");
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    e.handle_input("\x05");
    assert_eq!(e.get_cursor(), (0, 20));
    e.handle_input("\x1b[B");
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 10));
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (2, 10));
    e.handle_input("\x1b[A");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 10));
}
