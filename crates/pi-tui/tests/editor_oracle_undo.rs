//! Oracle editor tests — Undo
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
fn does_nothing_when_undo_stack_is_empty() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
}

#[test]
fn coalesces_consecutive_word_characters_into_one_undo_unit() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    assert_eq!(e.get_text(), "hello world");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
}

#[test]
fn undoes_spaces_one_at_a_time() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input(" ");
    assert_eq!(e.get_text(), "hello  ");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello ");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
}

#[test]
fn undoes_newlines_and_signals_next_word_to_capture_state() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input("\n");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    assert_eq!(e.get_text(), "hello\nworld");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello\n");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
}

#[test]
fn undoes_backspace() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input("\x7f");
    assert_eq!(e.get_text(), "hell");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello");
}

#[test]
fn undoes_forward_delete() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input("\x01");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[3~");
    assert_eq!(e.get_text(), "hllo");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello");
}

#[test]
fn undoes_ctrl_w_delete_word_backward() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    assert_eq!(e.get_text(), "hello world");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "hello ");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn undoes_ctrl_k_delete_to_line_end() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    e.handle_input("\x01");
    for _ in 0..6 { e.handle_input("\x1b[C"); }
    e.handle_input("\x0b");
    assert_eq!(e.get_text(), "hello ");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
    e.handle_input("|");
    assert_eq!(e.get_text(), "hello |world");
}

#[test]
fn undoes_ctrl_u_delete_to_line_start() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    e.handle_input("\x01");
    for _ in 0..6 { e.handle_input("\x1b[C"); }
    e.handle_input("\x15");
    assert_eq!(e.get_text(), "world");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn undoes_yank() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("\x17");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "hello ");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
}

#[test]
fn undoes_single_line_paste_atomically() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..5 { e.handle_input("\x1b[C"); }
    e.handle_input("\x1b[200~beep boop\x1b[201~");
    assert_eq!(e.get_text(), "hellobeep boop world");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
    e.handle_input("|");
    assert_eq!(e.get_text(), "hello| world");
}

#[test]
fn decodes_csi_u_ctrl_letter_sequences_inside_bracketed_paste_tmux_popup() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\x1b[200~line1\x1b[106;5uline2\x1b[106;5uline3\x1b[201~");
    assert_eq!(e.get_text(), "line1\nline2\nline3");
}

#[test]
fn undoes_multi_line_paste_atomically() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..5 { e.handle_input("\x1b[C"); }
    e.handle_input("\x1b[200~line1\nline2\nline3\x1b[201~");
    assert_eq!(e.get_text(), "helloline1\nline2\nline3 world");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
    e.handle_input("|");
    assert_eq!(e.get_text(), "hello| world");
}

#[test]
fn undoes_inserttextatcursor_atomically() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..5 { e.handle_input("\x1b[C"); }
    e.insert_text_at_cursor("/tmp/image.png");
    assert_eq!(e.get_text(), "hello/tmp/image.png world");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
    e.handle_input("|");
    assert_eq!(e.get_text(), "hello| world");
}

#[test]
fn inserttextatcursor_normalizes_crlf_and_cr_line_endings() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("");
    e.insert_text_at_cursor("a\r\nb\r\nc");
    assert_eq!(e.get_text(), "a\nb\nc");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
    e.insert_text_at_cursor("x\ry\rz");
    assert_eq!(e.get_text(), "x\ny\nz");
}

#[test]
fn undoes_settext_to_empty_string() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    assert_eq!(e.get_text(), "hello world");
    e.set_text("");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn exits_history_browsing_mode_on_undo() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("hello");
    assert_eq!(e.get_text(), "");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    assert_eq!(e.get_text(), "world");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "hello");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "world");
}

#[test]
fn undo_restores_to_pre_history_state_even_after_multiple_history_navigations() {
    let t = tui();
    let mut e = editor(&t);
    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("third");
    e.handle_input("c");
    e.handle_input("u");
    e.handle_input("r");
    e.handle_input("r");
    e.handle_input("e");
    e.handle_input("n");
    e.handle_input("t");
    assert_eq!(e.get_text(), "current");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "third");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "second");
    e.handle_input("\x1b[A");
    assert_eq!(e.get_text(), "first");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "current");
}

#[test]
fn cursor_movement_starts_new_undo_unit() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("w");
    e.handle_input("o");
    e.handle_input("r");
    e.handle_input("l");
    e.handle_input("d");
    assert_eq!(e.get_text(), "hello world");
    for _ in 0..5 { e.handle_input("\x1b[D"); }
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input("l");
    assert_eq!(e.get_text(), "hello lolworld");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
    e.handle_input("|");
    assert_eq!(e.get_text(), "hello |world");
}

#[test]
fn no_op_delete_operations_do_not_push_undo_snapshots() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("h");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    assert_eq!(e.get_text(), "hello");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x17");
    e.handle_input("\x17");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello");
}

#[test]
fn insert_text_at_cursor_handles_multiline_text() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..5 {
        e.handle_input("\x1b[C");
    }
    e.insert_text_at_cursor("|\nX");
    assert_eq!(e.get_text(), "hello|\nX world");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn clears_undo_stack_on_submit() {
    let t = tui();
    let mut e = editor(&t);
    let submitted = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let s2 = submitted.clone();
    e.on_submit = Some(Box::new(move |text| { *s2.borrow_mut() = text; }));
    e.handle_input("h");
    e.handle_input("i");
    e.handle_input("\r");
    assert_eq!(*submitted.borrow(), "hi");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
}

#[test]
fn does_not_trigger_autocomplete_during_single_line_paste() {
    struct P { calls: std::sync::Arc<std::sync::Mutex<usize>> }
    impl pi_tui::autocomplete::AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            _: &[String],
            _: usize,
            _: usize,
            _: pi_tui::autocomplete::SuggestionOptions,
        ) -> Option<pi_tui::autocomplete::AutocompleteSuggestions> {
            *self.calls.lock().unwrap() += 1;
            None
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &pi_tui::autocomplete::AutocompleteItem,
            prefix: &str,
        ) -> pi_tui::autocomplete::AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    e.set_autocomplete_provider(Box::new(P { calls: calls.clone() }));
    e.handle_input("\x1b[200~@file\x1b[201~");
    e.flush_autocomplete();
    assert_eq!(*calls.lock().unwrap(), 0);
    assert!(!e.is_showing_autocomplete());
}
