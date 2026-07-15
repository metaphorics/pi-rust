//! Oracle editor tests — Unicode text editing behavior
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
fn inserts_mixed_ascii_umlauts_and_emojis_as_literal_text() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("H");
    e.handle_input("e");
    e.handle_input("l");
    e.handle_input("l");
    e.handle_input("o");
    e.handle_input(" ");
    e.handle_input("\u{e4}");
    e.handle_input("\u{f6}");
    e.handle_input("\u{fc}");
    e.handle_input(" ");
    e.handle_input("\u{1f600}");
    let text = e.get_text();
    assert_eq!(text, "Hello \u{e4}\u{f6}\u{fc} \u{1f600}");
}

#[test]
fn deletes_single_code_unit_unicode_characters_umlauts_with_backspace() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\u{e4}");
    e.handle_input("\u{f6}");
    e.handle_input("\u{fc}");
    e.handle_input("\x7f");
    let text = e.get_text();
    assert_eq!(text, "\u{e4}\u{f6}");
}

#[test]
fn deletes_multi_code_unit_emojis_with_single_backspace() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\u{1f600}");
    e.handle_input("\u{1f44d}");
    e.handle_input("\x7f");
    let text = e.get_text();
    assert_eq!(text, "\u{1f600}");
}

#[test]
fn inserts_characters_at_the_correct_position_after_cursor_movement_over_umlauts() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\u{e4}");
    e.handle_input("\u{f6}");
    e.handle_input("\u{fc}");
    e.handle_input("\x1b[D");
    e.handle_input("\x1b[D");
    e.handle_input("x");
    let text = e.get_text();
    assert_eq!(text, "\u{e4}x\u{f6}\u{fc}");
}

#[test]
fn moves_cursor_across_multi_code_unit_emojis_with_single_arrow_key() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\u{1f600}");
    e.handle_input("\u{1f44d}");
    e.handle_input("\u{1f389}");
    e.handle_input("\x1b[D");
    e.handle_input("\x1b[D");
    e.handle_input("x");
    let text = e.get_text();
    assert_eq!(text, "\u{1f600}x\u{1f44d}\u{1f389}");
}

#[test]
fn preserves_umlauts_across_line_breaks() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("\u{e4}");
    e.handle_input("\u{f6}");
    e.handle_input("\u{fc}");
    e.handle_input("\n");
    e.handle_input("\u{c4}");
    e.handle_input("\u{d6}");
    e.handle_input("\u{dc}");
    let text = e.get_text();
    assert_eq!(text, "\u{e4}\u{f6}\u{fc}\n\u{c4}\u{d6}\u{dc}");
}

#[test]
fn replaces_the_entire_document_with_unicode_text_via_settext_paste_simulation() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("H\u{e4}ll\u{f6} W\u{f6}rld! \u{1f600} \u{e4}\u{f6}\u{fc}\u{c4}\u{d6}\u{dc}\u{df}");
    let text = e.get_text();
    assert_eq!(text, "H\u{e4}ll\u{f6} W\u{f6}rld! \u{1f600} \u{e4}\u{f6}\u{fc}\u{c4}\u{d6}\u{dc}\u{df}");
}

#[test]
fn moves_cursor_to_document_start_on_ctrl_a_and_inserts_at_the_beginning() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("a");
    e.handle_input("b");
    e.handle_input("\x01");
    e.handle_input("x");
    let text = e.get_text();
    assert_eq!(text, "xab");
}

#[test]
fn deletes_words_correctly_with_ctrl_w_and_alt_backspace() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("foo bar baz");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo bar ");
    e.set_text("foo bar   ");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo ");
    e.set_text("foo bar...");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo bar");
    e.set_text("foo.bar");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo.");
    e.set_text("foo:bar");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo:");
    e.set_text("line one\nline two");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "line one\nline ");
    e.set_text("line one\n");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "line one");
    e.set_text("foo \u{1f600}\u{1f600} bar");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo \u{1f600}\u{1f600} ");
    e.handle_input("\x17");
    assert_eq!(e.get_text(), "foo ");
    e.set_text("foo bar");
    e.handle_input("\x1b\x7f");
    assert_eq!(e.get_text(), "foo ");
}

#[test]
fn navigates_words_correctly_with_ctrl_left_right() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("foo bar... baz");
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 11));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 7));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 4));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 7));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 10));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 14));
    e.set_text("   foo bar");
    e.handle_input("\x01");
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 6));
    e.set_text("foo.bar baz");
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 8));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 4));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 3));
    e.handle_input("\x01");
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 3));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 4));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 7));
}

#[test]
fn stops_at_fullwidth_chinese_punctuation_issue_4972() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("\u{4f60}\u{597d}\u{ff0c}\u{4e16}\u{754c}");
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 3));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 2));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 2));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 3));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 5));
}

#[test]
fn handles_mixed_cjk_and_ascii_word_movement() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("hello\u{4f60}\u{597d}\u{ff0c}world\u{4e16}\u{754c}");
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 13));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 8));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 7));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 5));
    e.handle_input("\x1b[1;5D");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 5));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 7));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 8));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 13));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 15));
}
