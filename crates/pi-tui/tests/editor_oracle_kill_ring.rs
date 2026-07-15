//! Oracle editor tests — Kill ring
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
fn ctrl_w_saves_deleted_text_to_kill_ring_and_ctrl_y_yanks_it() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("foo bar baz");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo bar ");
    e.handle_input("\x01");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "bazfoo bar ");
}

#[test]
fn ctrl_u_saves_deleted_text_to_kill_ring() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[C");
    e.handle_input("\x15");
    assert_eq!(e.get_text(), "world");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn ctrl_k_saves_deleted_text_to_kill_ring() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world");
    e.handle_input("\x01");
    e.handle_input("\x0b");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn ctrl_y_does_nothing_when_kill_ring_is_empty() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("test");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "test");
}

#[test]
fn alt_y_cycles_through_kill_ring_after_ctrl_y() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("first");
    e.handle_input("\x17");
    e.set_text("second");
    e.handle_input("\x17");
    e.set_text("third");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "third");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "second");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "first");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "third");
}

#[test]
fn alt_y_does_nothing_if_not_preceded_by_yank() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("test");
    e.handle_input("\x17");
    e.set_text("other");
    e.handle_input("x");
    assert_eq!(e.get_text(), "otherx");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "otherx");
}

#[test]
fn alt_y_does_nothing_if_kill_ring_has_1_entry() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("only");
    e.handle_input("\x17");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "only");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "only");
}

#[test]
fn consecutive_ctrl_w_accumulates_into_one_kill_ring_entry() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("one two three");
    e.handle_input("\x17");
    e.handle_input("\x17");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "one two three");
}

#[test]
fn ctrl_u_accumulates_multiline_deletes_including_newlines() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("line1\nline2\nline3");
    e.handle_input("\x15");
    assert_eq!(e.get_text(), "line1\nline2\n");
    e.handle_input("\x15");
    assert_eq!(e.get_text(), "line1\nline2");
    e.handle_input("\x15");
    assert_eq!(e.get_text(), "line1\n");
    e.handle_input("\x15");
    assert_eq!(e.get_text(), "line1");
    e.handle_input("\x15");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "line1\nline2\nline3");
}

#[test]
fn backward_deletions_prepend_forward_deletions_append_during_accumulation() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("prefix|suffix");
    e.handle_input("\x01");
    for _ in 0..6 { e.handle_input("\x1b[C"); }
    e.handle_input("\x0b");
    e.handle_input("\x0b");
    assert_eq!(e.get_text(), "prefix");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "prefix|suffix");
}

#[test]
fn non_delete_actions_break_kill_accumulation() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("foo bar baz");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo bar ");
    e.handle_input("x");
    assert_eq!(e.get_text(), "foo bar x");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo bar ");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "foo bar x");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "foo bar baz");
}

#[test]
fn non_yank_actions_break_alt_y_chain() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("first");
    e.handle_input("\x17");
    e.set_text("second");
    e.handle_input("\x17");
    e.set_text("");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "second");
    e.handle_input("x");
    assert_eq!(e.get_text(), "secondx");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "secondx");
}

#[test]
fn kill_ring_rotation_persists_after_cycling() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("first");
    e.handle_input("\x17");
    e.set_text("second");
    e.handle_input("\x17");
    e.set_text("third");
    e.handle_input("\x17");
    e.set_text("");
    e.handle_input("\x19");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "second");
    e.handle_input("x");
    e.set_text("");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "second");
}

#[test]
fn consecutive_deletions_across_lines_coalesce_into_one_entry() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1\n2\n3");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "1\n2\n");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "1\n2");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "1\n");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "1");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "1\n2\n3");
}

#[test]
fn ctrl_k_at_line_end_deletes_newline_and_coalesces() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("");
    e.handle_input("a");
    e.handle_input("b");
    e.handle_input("\n");
    e.handle_input("c");
    e.handle_input("d");
    e.handle_input("\x1b[A");
    e.handle_input("\x05");
    e.handle_input("\x0b");
    assert_eq!(e.get_text(), "abcd");
    e.handle_input("\x0b");
    assert_eq!(e.get_text(), "ab");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "ab\ncd");
}

#[test]
fn handles_yank_in_middle_of_text() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("word");
    e.handle_input("\x17");
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..6 { e.handle_input("\x1b[C"); }
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "hello wordworld");
}

#[test]
fn handles_yank_pop_in_middle_of_text() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("FIRST");
    e.handle_input("\x17");
    e.set_text("SECOND");
    e.handle_input("\x17");
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..6 { e.handle_input("\x1b[C"); }
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "hello SECONDworld");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "hello FIRSTworld");
}

#[test]
fn multiline_yank_and_yank_pop_in_middle_of_text() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("SINGLE");
    e.handle_input("\x17");
    e.set_text("A\nB");
    e.handle_input("\x15");
    e.handle_input("\x15");
    e.handle_input("\x15");
    e.set_text("hello world");
    e.handle_input("\x01");
    for _ in 0..6 { e.handle_input("\x1b[C"); }
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "hello A\nBworld");
    e.handle_input("\x1by");
    assert_eq!(e.get_text(), "hello SINGLEworld");
}

#[test]
fn alt_d_deletes_word_forward_and_saves_to_kill_ring() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello world test");
    e.handle_input("\x01");
    e.handle_input("\x1bd");
    assert_eq!(e.get_text(), " world test");
    e.handle_input("\x1bd");
    assert_eq!(e.get_text(), " test");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "hello world test");
}

#[test]
fn alt_d_at_end_of_line_deletes_newline() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("line1\nline2");
    e.handle_input("\x1b[A");
    e.handle_input("\x05");
    e.handle_input("\x1bd");
    assert_eq!(e.get_text(), "line1line2");
    e.handle_input("\x19");
    assert_eq!(e.get_text(), "line1\nline2");
}
