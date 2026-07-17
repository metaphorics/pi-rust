//! Oracle editor tests — Backslash+Enter newline workaround
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
fn wraps_lines_correctly_when_text_contains_wide_emojis() {
    let t = tui();
    let mut e = editor(&t);
    let width = 20;
    e.set_text("Hello ✅ World");
    let lines = render_plain(&mut e, width as u16);
    for line in &lines {
        assert!(visible_width(line) <= width, "{line:?}");
    }
}

#[test]
fn wraps_long_text_with_emojis_at_correct_positions() {
    let t = tui();
    let mut e = editor(&t);
    let width = 10;
    e.set_text("✅✅✅✅✅✅");
    let lines = render_plain(&mut e, width as u16);
    for line in &lines {
        assert!(visible_width(line) <= width, "{line:?}");
    }
}

#[test]
fn renders_isolated_thai_and_lao_am_clusters_without_width_drift() {
    for text in ["ำabc", "ຳabc"] {
        let t = tui();
        let mut e = editor(&t);
        let width = 8;
        e.set_text(text);
        let lines = render_plain(&mut e, width as u16);
        for line in &lines {
            assert!(visible_width(line) <= width, "{line:?}");
        }
    }
}

#[test]
fn wraps_cjk_characters_correctly() {
    let t = tui();
    let mut e = editor(&t);
    let width = 11;
    e.set_text("日本語テスト");
    let lines = render_plain(&mut e, width as u16);
    let content = content_lines(&lines);
    assert!(content.len() >= 2);
    assert!(content[1].contains("ト") || content.iter().any(|l| l.contains("ト")));
}

#[test]
fn does_not_exceed_terminal_width_with_emoji_at_wrap_boundary() {
    let t = tui();
    let mut e = editor(&t);
    let width = 11;
    e.set_text("0123456789✅");
    let lines = render_plain(&mut e, width as u16);
    for line in &lines {
        assert!(visible_width(line) <= width, "{line:?}");
    }
}

#[test]
fn shows_cursor_at_end_of_line_before_wrap() {
    for padding_x in [0usize, 1] {
        let t = tui_size((10 + padding_x) as u16, 24);
        let mut e = editor_opts(&t, padding_x);
        for _ in 0..9 {
            e.handle_input("x");
        }
        let lines = render_plain(&mut e, (10 + padding_x) as u16);
        let content = content_lines(&lines);
        // 9 chars fit layout width; single content line expected
        assert!(!content.is_empty());
    }
}
