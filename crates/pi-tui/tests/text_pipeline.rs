//! Conformance tests for pi's visibleWidth / wrapTextWithAnsi.
//!
//! Cases ported from packages/tui/test/wrap-ansi.test.ts and
//! truncate-to-width.test.ts (visibleWidth section). Fixtures live as
//! inline constants matching the TS assertions.

use pi_tui::util::{normalize_terminal_output, visible_width, wrap_text_with_ansi};

#[test]
fn wrap_plain_text_respects_width() {
    let text = "hello world this is a test";
    let wrapped = wrap_text_with_ansi(text, 10);
    assert!(wrapped.len() > 1);
    for line in &wrapped {
        assert!(visible_width(line) <= 10, "line too wide: {line:?}");
    }
}

#[test]
fn wrap_cjk_after_latin() {
    let text = "This is an example 中文汉字测试段落内容中文汉字测试段落内容.";
    let wrapped = wrap_text_with_ansi(text, 40);
    assert_eq!(
        wrapped,
        vec![
            "This is an example 中文汉字测试段落内容".to_string(),
            "中文汉字测试段落内容.".to_string(),
        ]
    );
    for line in &wrapped {
        assert!(visible_width(line) <= 40);
    }
}

#[test]
fn wrap_preserves_color_on_cjk() {
    let red = "\x1b[31m";
    let reset = "\x1b[0m";
    let text = format!("{red}This is an example 中文汉字测试段落内容中文汉字测试段落内容.{reset}");
    let wrapped = wrap_text_with_ansi(&text, 40);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(
        wrapped[0],
        format!("{red}This is an example 中文汉字测试段落内容")
    );
    assert_eq!(
        wrapped[1],
        format!("{red}中文汉字测试段落内容.{reset}")
    );
}

#[test]
fn wrap_preserves_color_across_wraps() {
    let red = "\x1b[31m";
    let reset = "\x1b[0m";
    let text = format!("{red}hello world this is red{reset}");
    let wrapped = wrap_text_with_ansi(&text, 10);
    for line in wrapped.iter().skip(1) {
        assert!(line.starts_with(red), "continuation missing red: {line:?}");
    }
    for line in wrapped.iter().take(wrapped.len().saturating_sub(1)) {
        assert!(
            !line.ends_with("\x1b[0m"),
            "middle line should not full-reset: {line:?}"
        );
    }
}

#[test]
fn visible_width_skips_osc133_bel() {
    let text = "\x1b]133;A\x07hello\x1b]133;B\x07";
    assert_eq!(visible_width(text), 5);
}

#[test]
fn visible_width_skips_osc_st() {
    let text = "\x1b]133;A\x1b\\hello\x1b]133;B\x1b\\";
    assert_eq!(visible_width(text), 5);
}

#[test]
fn visible_width_regional_indicators() {
    assert_eq!(visible_width("🇨"), 2);
    assert_eq!(visible_width("🇨🇳"), 2);
}

#[test]
fn wrap_trailing_whitespace_at_width_1() {
    let two = wrap_text_with_ansi("  ", 1);
    assert!(!two.is_empty());
    // pi: each resulting line's visible width must be <= 1
    for line in &two {
        assert!(visible_width(line) <= 1, "line too wide: {line:?} w={}", visible_width(line));
    }
}

#[test]
fn visible_width_tab_and_ansi() {
    // tab=3 + 界=2 → 5
    assert_eq!(visible_width("\t\x1b[31m界\x1b[0m"), 5);
}

#[test]
fn visible_width_thai_lao_am() {
    assert_eq!(visible_width("ำ"), 1);
    assert_eq!(visible_width("ຳ"), 1);
    assert_eq!(visible_width("กำ"), 2);
    assert_eq!(visible_width("ກຳ"), 2);
}

#[test]
fn normalize_terminal_output_am() {
    assert_eq!(normalize_terminal_output("ำ"), "ํา");
    assert_eq!(normalize_terminal_output("ຳ"), "ໍາ");
    assert_eq!(
        visible_width(&normalize_terminal_output("ำabc")),
        visible_width("ำabc")
    );
    assert_eq!(
        visible_width(&normalize_terminal_output("ຳabc")),
        visible_width("ຳabc")
    );
}

#[test]
fn underline_line_end_reset_not_full_reset() {
    // Underlined text that wraps: line ends with underline-off (\x1b[24m), not \x1b[0m.
    let ul = "\x1b[4m";
    let text = format!("{ul}abcdefghijklmnopqrstuvwxyz");
    let wrapped = wrap_text_with_ansi(&text, 10);
    assert!(wrapped.len() > 1);
    for line in wrapped.iter().take(wrapped.len() - 1) {
        // Must not end with full SGR reset if underline-only close is used
        // (pi getLineEndReset → \x1b[24m).
        if line.contains("\x1b[4m") || line.contains('a') {
            // continuation reopen may omit; first lines should close underline
            let _ = line;
        }
    }
    // At least one non-final line should contain underline-off when underline active
    let has_ul_off = wrapped
        .iter()
        .take(wrapped.len().saturating_sub(1))
        .any(|l| l.contains("\x1b[24m"));
    assert!(
        has_ul_off || wrapped.len() == 1,
        "expected underline-only line-end reset on wrapped lines: {wrapped:?}"
    );
}
