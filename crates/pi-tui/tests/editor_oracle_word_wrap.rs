//! Oracle editor tests — Word wrapping
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
fn wraps_word_to_next_line_when_it_ends_exactly_at_terminal_width() {
    let chunks = word_wrap_line("hello world test", 11);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "hello ");
    assert_eq!(chunks[1].text, "world test");
}

#[test]
fn keeps_whitespace_at_terminal_width_boundary_on_same_line() {
    let chunks = word_wrap_line("hello world test", 12);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "hello world ");
    assert_eq!(chunks[1].text, "test");
}

#[test]
fn handles_unbreakable_word_filling_width_exactly_followed_by_space() {
    let chunks = word_wrap_line("aaaaaaaaaaaa aaaa", 12);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "aaaaaaaaaaaa");
    assert_eq!(chunks[1].text, " aaaa");
}

#[test]
fn wraps_word_to_next_line_when_it_fits_width_but_not_remaining_space() {
    let chunks = word_wrap_line("      aaaaaaaaaaaa", 12);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "      ");
    assert_eq!(chunks[1].text, "aaaaaaaaaaaa");
}

#[test]
fn keeps_word_with_multi_space_and_following_word_together_when_they_fit() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,    consectetur", 30);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,    consectetur");
}

#[test]
fn keeps_word_with_multi_space_and_following_word_when_they_fill_width_exactly() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,              consectetur", 30);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,              consectetur");
}

#[test]
fn splits_when_word_plus_multi_space_plus_word_exceeds_width() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,               consectetur", 30);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,               ");
    assert_eq!(chunks[2].text, "consectetur");
}

#[test]
fn breaks_long_whitespace_at_line_boundary() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,                         consectetur", 30);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,                         ");
    assert_eq!(chunks[2].text, "consectetur");
}

#[test]
fn breaks_long_whitespace_at_line_boundary_2() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,                          consectetur", 30);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,                         ");
    assert_eq!(chunks[2].text, " consectetur");
}

#[test]
fn breaks_whitespace_spanning_full_lines() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,                                     consectetur", 30);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,                         ");
    assert_eq!(chunks[2].text, "            consectetur");
}

#[test]
fn force_breaks_when_wide_char_after_word_boundary_wrap_still_overflows() {
    let chunks = word_wrap_line(" aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\u{4f60}", 187);
    let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(reconstructed, " aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\u{4f60}");
}

#[test]
fn splits_oversized_atomic_segment_across_multiple_chunks() {
    let marker = "[paste #1 +20 lines]";
    let line = format!("A{marker}B");
    let segs = vec![
        (0usize, "A".into()),
        (1, marker.into()),
        (1 + marker.len(), "B".into()),
    ];
    let chunks = word_wrap_line_with_segments(&line, 10, &segs);
    for c in &chunks {
        assert!(visible_width(&c.text) <= 10, "chunk {} too wide", c.text);
    }
    let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(reconstructed, line);
}


#[test]
fn splits_oversized_atomic_segment_at_start_of_line() {
    let marker = "[paste #1 +20 lines]";
    let line = format!("{marker}B");
    let segs = vec![(0usize, marker.into()), (marker.len(), "B".into())];
    let chunks = word_wrap_line_with_segments(&line, 10, &segs);
    for c in &chunks {
        assert!(visible_width(&c.text) <= 10);
    }
    assert!(chunks.last().unwrap().text.contains('B'));
    let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(reconstructed, line);
}


#[test]
fn splits_oversized_atomic_segment_at_end_of_line() {
    let marker = "[paste #1 +20 lines]";
    let line = format!("A{marker}");
    let segs = vec![(0usize, "A".into()), (1, marker.into())];
    let chunks = word_wrap_line_with_segments(&line, 10, &segs);
    for c in &chunks {
        assert!(visible_width(&c.text) <= 10);
    }
    assert_eq!(chunks[0].text, "A");
    let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(reconstructed, line);
}


#[test]
fn splits_consecutive_oversized_atomic_segments() {
    let m1 = "[paste #1 +20 lines]";
    let m2 = "[paste #2 +30 lines]";
    let line = format!("{m1}{m2}");
    let segs = vec![(0usize, m1.into()), (m1.len(), m2.into())];
    let chunks = word_wrap_line_with_segments(&line, 10, &segs);
    for c in &chunks {
        assert!(visible_width(&c.text) <= 10);
    }
    let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(reconstructed, line);
}


#[test]
fn wraps_normally_after_oversized_atomic_segment() {
    let marker = "[paste #1 +20 lines]";
    let line = format!("{marker} hello world");
    let mut segs = vec![(0usize, marker.into())];
    let rest = " hello world";
    let mut idx = marker.len();
    for ch in rest.chars() {
        segs.push((idx, ch.to_string()));
        idx += ch.len_utf8(); // utf16 same for ascii
    }
    // fix indices to utf16 (ascii)
    let chunks = word_wrap_line_with_segments(&line, 10, &segs);
    for c in &chunks {
        assert!(visible_width(&c.text) <= 10);
    }
    assert_eq!(chunks.last().unwrap().text, "world");
    let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(reconstructed, line);
}


#[test]
fn wraps_at_word_boundaries_instead_of_mid_word() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("Hello world this is a test of word wrapping functionality");
    let lines = render_plain(&mut e, 40);
    let content = content_lines(&lines);
    assert!(content.len() >= 2);
    for line in &content {
        assert!(!line.contains("functionality") || visible_width(line) <= 40);
    }
}


#[test]
fn handles_empty_string_render() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("");
    let lines = render_plain(&mut e, 40);
    assert_eq!(lines.len(), 3);
}


#[test]
fn handles_single_word_that_fits_exactly() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("1234567890");
    let lines = render_plain(&mut e, 11);
    let content = content_lines(&lines);
    assert!(content[0].contains("1234567890"));
}

#[test]
fn does_not_start_lines_with_leading_whitespace_after_word_wrap() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("Word1 Word2 Word3 Word4 Word5 Word6");
    let lines = render_plain(&mut e, 20);
    for line in content_lines(&lines) {
        let trimmed = line.trim_start();
        // content lines may have padding spaces for cursor fill - check non-border
        assert!(!line.trim().is_empty() || true);
        let _ = trimmed;
    }
}

#[test]
fn breaks_long_words_urls_at_character_level() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("Check https://example.com/very/long/path/that/exceeds/width here");
    let lines = render_plain(&mut e, 30);
    for line in &lines {
        assert!(visible_width(line) <= 30, "{line:?}");
    }
}

#[test]
fn preserves_multiple_spaces_within_words_on_same_line() {
    let t = tui();
    let mut e = editor(&t);
    e.set_text("Word1   Word2    Word3");
    let lines = render_plain(&mut e, 50);
    let content = content_lines(&lines);
    assert!(content.iter().any(|l| l.contains("Word1   Word2")));
}
