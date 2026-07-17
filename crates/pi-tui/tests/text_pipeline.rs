//! Conformance tests for pi's visibleWidth / wrapTextWithAnsi.
//!
//! Cases ported from packages/tui/test/wrap-ansi.test.ts and
//! truncate-to-width.test.ts (visibleWidth section). Fixtures live as
//! inline constants matching the TS assertions (no separate fixtures/
//! payload — the empty dir was removed).

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
    assert_eq!(wrapped[1], format!("{red}中文汉字测试段落内容.{reset}"));
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
        assert!(
            visible_width(line) <= 1,
            "line too wide: {line:?} w={}",
            visible_width(line)
        );
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
    // Underlined text that wraps: non-final lines MUST end with underline-off
    // (\x1b[24m) from getLineEndReset, NOT full SGR reset (\x1b[0m).
    let ul = "\x1b[4m";
    let text = format!("{ul}abcdefghijklmnopqrstuvwxyz");
    let wrapped = wrap_text_with_ansi(&text, 10);
    assert!(wrapped.len() > 1, "expected wrap: {wrapped:?}");
    for (i, line) in wrapped.iter().enumerate().take(wrapped.len() - 1) {
        assert!(
            line.ends_with("\x1b[24m"),
            "non-final line {i} must end with underline-only reset \\x1b[24m, got {line:?}"
        );
        assert!(
            !line.ends_with("\x1b[0m"),
            "non-final line {i} must not end with full SGR reset: {line:?}"
        );
        // Byte-strict: the last 5 bytes of a non-final underlined wrap are ESC [ 2 4 m
        let bytes = line.as_bytes();
        assert!(
            bytes.len() >= 5 && &bytes[bytes.len() - 5..] == b"\x1b[24m",
            "byte-strict underline-off suffix missing on line {i}: {line:?}"
        );
    }
}

#[test]
fn wrap_final_trim_end_strips_trailing_spaces() {
    // Trailing whitespace must not push a line over width (utils.ts final trimEnd).
    let text = "hello     world";
    let wrapped = wrap_text_with_ansi(text, 8);
    for line in &wrapped {
        assert!(
            visible_width(line) <= 8,
            "line exceeds width after trimEnd: {line:?} (vw={})",
            visible_width(line)
        );
        assert!(!line.ends_with(' '), "trailing space left: {line:?}");
    }
}

#[test]
fn wrap_long_whitespace_token_not_hard_broken() {
    // Whitespace-only tokens wider than width must not enter breakLongWord
    // (utils.ts:741 `&& !isWhitespace`).
    let spaces = " ".repeat(20);
    let text = format!("ab{spaces}cd");
    let wrapped = wrap_text_with_ansi(&text, 5);
    // Should still wrap words without panicking / producing empty garbage lines.
    assert!(!wrapped.is_empty());
    for line in &wrapped {
        assert!(visible_width(line) <= 5, "line {line:?}");
    }
}
