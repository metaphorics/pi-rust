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
fn creates_a_paste_marker_for_large_pastes() {
    let t = tui();
    let mut e = editor(&t);
    let text = paste_with_marker(&mut e);
    assert!(
        regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]")
            .unwrap()
            .is_match(&text)
    );
}

#[test]
fn treats_paste_marker_as_single_unit_for_right_arrow() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("A");
    paste_with_marker(&mut e);
    e.handle_input("B");
    e.handle_input("");
    assert_eq!(e.get_cursor(), (0, 0));
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (0, 1));
    e.handle_input("\x1b[C");
    let marker = regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]")
        .unwrap()
        .find(&e.get_text())
        .unwrap()
        .as_str()
        .to_owned();
    assert_eq!(e.get_cursor(), (0, 1 + marker.len()));
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (0, 1 + marker.len() + 1));
}

#[test]
fn treats_paste_marker_as_single_unit_for_left_arrow() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("A");
    paste_with_marker(&mut e);
    e.handle_input("B");
    let text = e.get_text();
    let marker = regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]")
        .unwrap()
        .find(&text)
        .unwrap()
        .as_str()
        .to_owned();
    e.handle_input("\x1b[D");
    assert_eq!(e.get_cursor(), (0, 1 + marker.len()));
    e.handle_input("\x1b[D");
    assert_eq!(e.get_cursor(), (0, 1));
    e.handle_input("\x1b[D");
    assert_eq!(e.get_cursor(), (0, 0));
}

#[test]
fn treats_paste_marker_as_single_unit_for_backspace() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("A");
    paste_with_marker(&mut e);
    e.handle_input("B");
    let text = e.get_text();
    let marker = regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]")
        .unwrap()
        .find(&text)
        .unwrap()
        .as_str()
        .to_owned();
    e.handle_input("");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (0, 1 + marker.len()));
    e.handle_input("");
    assert_eq!(e.get_text(), "AB");
    assert_eq!(e.get_cursor(), (0, 1));
}

#[test]
fn treats_paste_marker_as_single_unit_for_forward_delete() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("A");
    paste_with_marker(&mut e);
    e.handle_input("B");
    e.handle_input("");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[3~");
    assert_eq!(e.get_text(), "AB");
    assert_eq!(e.get_cursor(), (0, 1));
}

#[test]
fn does_not_treat_manually_typed_marker_like_text_as_atomic() {
    let t = tui();
    let mut e = editor(&t);
    let fake = "[paste #99 +5 lines]";
    for ch in fake.chars() {
        e.handle_input(&ch.to_string());
    }
    assert_eq!(e.get_text(), fake);
    e.handle_input("");
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (0, 1));
}

#[test]
fn expands_large_pasted_content_literally_in_getexpandedtext() {
    let t = tui();
    let mut e = editor(&t);
    let pasted = [
        "line 1",
        "line 2",
        "line 3",
        "line 4",
        "line 5",
        "line 6",
        "line 7",
        "line 8",
        "line 9",
        "line 10",
        "tokens $1 $2 $& $$ $` $' end",
    ]
    .join("\n");
    e.handle_input(&format!("\x1b[200~{pasted}\x1b[201~"));
    assert!(
        regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]")
            .unwrap()
            .is_match(&e.get_text())
    );
    assert_eq!(e.get_expanded_text(), pasted);
}

#[test]
fn submits_large_pasted_content_literally() {
    let t = tui();
    let mut e = editor(&t);
    let pasted = [
        "line 1",
        "line 2",
        "line 3",
        "line 4",
        "line 5",
        "line 6",
        "line 7",
        "line 8",
        "line 9",
        "line 10",
        "tokens $1 $2 $& $$ $` $' end",
    ]
    .join("\n");
    let submitted = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let s2 = submitted.clone();
    e.on_submit = Some(Box::new(move |text| {
        *s2.borrow_mut() = text;
    }));
    e.handle_input(&format!("\x1b[200~{pasted}\x1b[201~"));
    e.handle_input("\r");
    assert_eq!(*submitted.borrow(), pasted);
}

#[test]
fn does_not_crash_when_paste_marker_wider_than_terminal() {
    let t = tui();
    let mut e = editor(&t);
    let big = "line\n".repeat(47);
    let big = big.trim_end();
    e.handle_input(&format!("\x1b[200~{big}\x1b[201~"));
    let text = e.get_text();
    let marker = regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]")
        .unwrap()
        .find(&text)
        .unwrap()
        .as_str();
    assert!(visible_width(marker) > 8);
    let lines = render_plain(&mut e, 8);
    for line in lines {
        assert!(visible_width(&line) <= 8, "line exceeds width 8: {line:?}");
    }
}

#[test]
fn undo_restores_marker_after_backspace_deletion() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("A");
    paste_with_marker(&mut e);
    e.handle_input("B");
    let text_before = e.get_text();
    e.handle_input("\x01");
    e.handle_input("\x1b[C");
    e.handle_input("\x1b[C");
    e.handle_input("\x7f");
    assert_eq!(e.get_text(), "AB");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), text_before);
}

#[test]
fn handles_multiple_paste_markers_in_same_line() {
    let t = tui();
    let mut e = editor(&t);
    paste_with_marker(&mut e);
    e.handle_input(" ");
    paste_with_marker(&mut e);
    let text = e.get_text();
    let re = regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]").unwrap();
    let markers: Vec<_> = re.find_iter(&text).map(|m| m.as_str().to_owned()).collect();
    assert_eq!(markers.len(), 2);
    e.handle_input("\x01");
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (0, markers[0].len()));
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (0, markers[0].len() + 1));
    e.handle_input("\x1b[C");
    assert_eq!(e.get_cursor(), (0, markers[0].len() + 1 + markers[1].len()));
}

#[test]
fn treats_paste_marker_as_single_unit_for_word_movement() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input("X");
    e.handle_input(" ");
    paste_with_marker(&mut e);
    e.handle_input(" ");
    e.handle_input("Y");
    let text = e.get_text();
    let marker = regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]")
        .unwrap()
        .find(&text)
        .unwrap()
        .as_str()
        .to_owned();
    e.handle_input("\x01");
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 1));
    e.handle_input("\x1b[1;5C");
    assert_eq!(e.get_cursor(), (0, 2 + marker.len()));
}

#[test]
fn snaps_to_the_paste_marker_start_when_navigating_down_into_it() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("12345678901234567890\n\nhello ");
    let big = "x".repeat(2000);
    e.handle_input(&format!("\x1b[200~{big}\x1b[201~"));
    let _ = e.render(80);
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
    assert_eq!(e.get_cursor(), (2, 6));
}

/// Oracle: preserves sticky column when navigating through paste marker line.
#[test]
fn preserves_sticky_column_when_navigating_through_paste_marker_line() {
    let t = tui_size(30, 24);
    let mut e = editor(&t);

    // Line 0: "1234567890123456" (16 chars)
    // Line 1: "" (empty)
    // Line 2: paste marker (~22 chars)
    // Line 3: "" (empty)
    // Line 4: "abcdefghijklmnop" (16 chars)
    for ch in "1234567890123456".chars() {
        e.handle_input(&ch.to_string());
    }
    e.handle_input("\n");
    e.handle_input("\n");
    e.handle_input(&format!("\x1b[200~{}\x1b[201~", "x".repeat(2000)));
    e.handle_input("\n");
    e.handle_input("\n");
    for ch in "abcdefghijklmnop".chars() {
        e.handle_input(&ch.to_string());
    }
    let _ = e.render(30);

    // Navigate to line 0, col 10
    for _ in 0..4 {
        e.handle_input("\x1b[A");
    }
    e.handle_input("\x01");
    for _ in 0..10 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (0, 10));

    // Down to empty line - sticky col 10 established
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (1, 0));

    // Down to paste marker - cursor snapped to col 0 (start of marker)
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (2, 0));

    // Down to empty line
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (3, 0));

    // Down to last line - should restore sticky col 10
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (4, 10));
}

/// Oracle: does not get stuck moving down from a multi-visual-line paste marker.
#[test]
fn does_not_get_stuck_moving_down_from_a_multi_visual_line_paste_marker() {
    let t = tui_size(20, 24);
    let mut e = editor(&t);

    // Logical line 0: "abcdefgh" + marker(21 chars) + "ijklmnopqr"
    // Logical line 1: "123456789012345678"
    // Marker "[paste #1 +100 lines]" (21 chars) wider than terminal (20).
    for ch in "abcdefgh".chars() {
        e.handle_input(&ch.to_string());
    }
    let big_content = "line\n".repeat(100);
    let big_content = big_content.trim_end();
    e.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));
    for ch in "ijklmnopqr".chars() {
        e.handle_input(&ch.to_string());
    }
    e.handle_input("\n");
    for ch in "123456789012345678".chars() {
        e.handle_input(&ch.to_string());
    }
    let _ = e.render(20);

    let text = e.get_text();
    let re = regex::Regex::new(r"\[paste #\d+ \+\d+ lines\]").unwrap();
    let marker = re
        .find(&text)
        .expect("paste marker should be created")
        .as_str();
    let marker_len = utf16_len(marker); // 21
    assert!(marker_len > 20, "marker should be wider than terminal");
    let marker_start = 8usize;
    let marker_end = marker_start + marker_len; // 29

    // Navigate to line 0, col 6 (on "g"). Preferred col 6 is past the
    // marker tail on VL3, so the cursor should land on content ("i" at
    // col 29) without snapping back.
    e.handle_input("\x1b[A"); // Up to line 0
    e.handle_input("\x01"); // Ctrl+A
    for _ in 0..6 {
        e.handle_input("\x1b[C");
    }
    assert_eq!(e.get_cursor(), (0, 6));

    // Down: cursor lands on paste marker start
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor(), (0, marker_start));

    // Down again: preferred col 6 lands at VL3 col 29 ("i"), which is
    // past the marker. Cursor stays on line 0.
    e.handle_input("\x1b[B");
    assert_eq!(e.get_cursor().0, 0);
    assert_eq!(e.get_cursor().1, marker_end); // col 29 = "i"

    // Up: back to paste marker
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, marker_start));

    // Up again: back to col 6 ("g")
    e.handle_input("\x1b[A");
    assert_eq!(e.get_cursor(), (0, 6));
}

#[test]
fn does_not_crash_when_text_plus_paste_marker_exceeds_width_with_cursor_on_marker() {
    let t = tui();
    let mut e = editor(&t);
    for _ in 0..35 {
        e.handle_input("b");
    }
    let big = "line\n".repeat(27);
    let big = big.trim_end();
    e.handle_input(&format!("\x1b[200~{big}\x1b[201~"));
    for _ in 0..4 {
        e.handle_input("b");
    }
    for _ in 0..5 {
        e.handle_input("\x1b[D");
    }
    let render_width = 54u16;
    let lines = render_plain(&mut e, render_width);
    for line in lines {
        assert!(visible_width(&line) <= render_width as usize, "{line:?}");
    }
}

#[test]
fn word_wrap_rechecks_overflow_after_backtracking_to_wrap_opportunity() {
    let t = tui();
    let mut e = editor(&t);
    e.handle_input(" ");
    for _ in 0..35 {
        e.handle_input("b");
    }
    let big = "line\n".repeat(27);
    let big = big.trim_end();
    e.handle_input(&format!("\x1b[200~{big}\x1b[201~"));
    for _ in 0..4 {
        e.handle_input("b");
    }
    let render_width = 54u16;
    let lines = render_plain(&mut e, render_width);
    for line in lines {
        assert!(visible_width(&line) <= render_width as usize, "{line:?}");
    }
}
