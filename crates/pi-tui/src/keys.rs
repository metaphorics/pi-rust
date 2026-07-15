//! Keyboard matching — wholesale port of packages/tui/src/keys.ts.
//!
//! Matches RAW byte sequences with mode-dependent ambiguity when Kitty
//! protocol is active. Not inkferro-core keypress (different parser).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set global Kitty keyboard protocol state (ProcessTerminal after detect).
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::SeqCst);
}

pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::SeqCst)
}

const MOD_SHIFT: u32 = 1;
const MOD_ALT: u32 = 2;
const MOD_CTRL: u32 = 4;
const MOD_SUPER: u32 = 8;
const LOCK_MASK: u32 = 64 + 128;

const CP_ESCAPE: i32 = 27;
const CP_TAB: i32 = 9;
const CP_ENTER: i32 = 13;
const CP_SPACE: i32 = 32;
const CP_BACKSPACE: i32 = 127;
const CP_KP_ENTER: i32 = 57414;

const ARROW_UP: i32 = -1;
const ARROW_DOWN: i32 = -2;
const ARROW_RIGHT: i32 = -3;
const ARROW_LEFT: i32 = -4;

const FN_DELETE: i32 = -10;
const FN_INSERT: i32 = -11;
const FN_PAGE_UP: i32 = -12;
const FN_PAGE_DOWN: i32 = -13;
const FN_HOME: i32 = -14;
const FN_END: i32 = -15;


fn is_symbol_key(s: &str) -> bool {
    matches!(
        s,
        "`" | "-"
            | "="
            | "["
            | "]"
            | "\\"
            | ";"
            | "'"
            | ","
            | "."
            | "/"
            | "!"
            | "@"
            | "#"
            | "$"
            | "%"
            | "^"
            | "&"
            | "*"
            | "("
            | ")"
            | "_"
            | "+"
            | "|"
            | "~"
            | "{"
            | "}"
            | ":"
            | "<"
            | ">"
            | "?"
    )
}

fn normalize_kitty_functional(cp: i32) -> i32 {
    match cp {
        57399 => 48,
        57400 => 49,
        57401 => 50,
        57402 => 51,
        57403 => 52,
        57404 => 53,
        57405 => 54,
        57406 => 55,
        57407 => 56,
        57408 => 57,
        57409 => 46,
        57410 => 47,
        57411 => 42,
        57412 => 45,
        57413 => 43,
        57415 => 61,
        57416 => 44,
        57417 => ARROW_LEFT,
        57418 => ARROW_RIGHT,
        57419 => ARROW_UP,
        57420 => ARROW_DOWN,
        57421 => FN_PAGE_UP,
        57422 => FN_PAGE_DOWN,
        57423 => FN_HOME,
        57424 => FN_END,
        57425 => FN_INSERT,
        57426 => FN_DELETE,
        other => other,
    }
}

fn normalize_shifted_letter(cp: i32, modifier: u32) -> i32 {
    let effective = modifier & !LOCK_MASK;
    if (effective & MOD_SHIFT) != 0 && (65..=90).contains(&cp) {
        cp + 32
    } else {
        cp
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone)]
struct ParsedKitty {
    codepoint: i32,
    #[allow(dead_code)]
    shifted_key: Option<i32>,
    base_layout_key: Option<i32>,
    modifier: u32,
    #[allow(dead_code)]
    event_type: KeyEventType,
}

fn parse_event_type(s: Option<&str>) -> KeyEventType {
    match s.and_then(|x| x.parse::<u32>().ok()) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

fn parse_kitty_sequence(data: &str) -> Option<ParsedKitty> {
    static CSI_U: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^\x1b\[(\d+)(?::(\d*))?(?::(\d+))?(?:;(\d+))?(?::(\d+))?u$").unwrap()
    });
    if let Some(caps) = CSI_U.captures(data) {
        let codepoint: i32 = caps[1].parse().ok()?;
        let shifted = caps
            .get(2)
            .filter(|m| !m.as_str().is_empty())
            .and_then(|m| m.as_str().parse().ok());
        let base = caps.get(3).and_then(|m| m.as_str().parse().ok());
        let mod_value: u32 = caps
            .get(4)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(1);
        let event = parse_event_type(caps.get(5).map(|m| m.as_str()));
        return Some(ParsedKitty {
            codepoint,
            shifted_key: shifted,
            base_layout_key: base,
            modifier: mod_value.saturating_sub(1),
            event_type: event,
        });
    }

    // Arrows with mod: \x1b[1;<mod>(:<event>)?A/B/C/D
    static ARROW: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\x1b\[1;(\d+)(?::(\d+))?([ABCD])$").unwrap());
    if let Some(caps) = ARROW.captures(data) {
        let mod_value: u32 = caps[1].parse().ok()?;
        let event = parse_event_type(caps.get(2).map(|m| m.as_str()));
        let cp = match &caps[3] {
            "A" => ARROW_UP,
            "B" => ARROW_DOWN,
            "C" => ARROW_RIGHT,
            "D" => ARROW_LEFT,
            _ => return None,
        };
        return Some(ParsedKitty {
            codepoint: cp,
            shifted_key: None,
            base_layout_key: None,
            modifier: mod_value.saturating_sub(1),
            event_type: event,
        });
    }

    // Functional ~ keys
    static FUNC: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\x1b\[(\d+)(?:;(\d+))?(?::(\d+))?~$").unwrap());
    if let Some(caps) = FUNC.captures(data) {
        let key_num: u32 = caps[1].parse().ok()?;
        let mod_value: u32 = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(1);
        let event = parse_event_type(caps.get(3).map(|m| m.as_str()));
        let cp = match key_num {
            2 => FN_INSERT,
            3 => FN_DELETE,
            5 => FN_PAGE_UP,
            6 => FN_PAGE_DOWN,
            7 => FN_HOME,
            8 => FN_END,
            _ => return None,
        };
        return Some(ParsedKitty {
            codepoint: cp,
            shifted_key: None,
            base_layout_key: None,
            modifier: mod_value.saturating_sub(1),
            event_type: event,
        });
    }

    // Home/End with mod
    static HOME_END: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\x1b\[1;(\d+)(?::(\d+))?([HF])$").unwrap());
    if let Some(caps) = HOME_END.captures(data) {
        let mod_value: u32 = caps[1].parse().ok()?;
        let event = parse_event_type(caps.get(2).map(|m| m.as_str()));
        let cp = if &caps[3] == "H" { FN_HOME } else { FN_END };
        return Some(ParsedKitty {
            codepoint: cp,
            shifted_key: None,
            base_layout_key: None,
            modifier: mod_value.saturating_sub(1),
            event_type: event,
        });
    }

    None
}

fn matches_kitty_sequence(data: &str, expected_cp: i32, expected_mod: u32) -> bool {
    let Some(parsed) = parse_kitty_sequence(data) else {
        return false;
    };
    let actual_mod = parsed.modifier & !LOCK_MASK;
    let expected_mod = expected_mod & !LOCK_MASK;
    if actual_mod != expected_mod {
        return false;
    }
    let normalized = normalize_shifted_letter(
        normalize_kitty_functional(parsed.codepoint),
        parsed.modifier,
    );
    let expected = normalize_shifted_letter(normalize_kitty_functional(expected_cp), expected_mod);
    if normalized == expected {
        return true;
    }
    // baseLayoutKey fallback for non-Latin layouts
    if let Some(base) = parsed.base_layout_key
        && base == expected_cp {
            let is_latin = (97..=122).contains(&normalized);
            let is_symbol = char::from_u32(normalized as u32)
                .map(|c| is_symbol_key(&c.to_string()))
                .unwrap_or(false);
            if !is_latin && !is_symbol {
                return true;
            }
        }
    false
}

#[derive(Debug, Clone, Copy)]
struct ParsedMok {
    codepoint: i32,
    modifier: u32,
}

fn parse_modify_other_keys(data: &str) -> Option<ParsedMok> {
    static RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\x1b\[27;(\d+);(\d+)~$").unwrap());
    let caps = RE.captures(data)?;
    let mod_value: u32 = caps[1].parse().ok()?;
    let codepoint: i32 = caps[2].parse().ok()?;
    Some(ParsedMok {
        codepoint,
        modifier: mod_value.saturating_sub(1),
    })
}

fn matches_modify_other_keys(data: &str, expected_cp: i32, expected_mod: u32) -> bool {
    let Some(parsed) = parse_modify_other_keys(data) else {
        return false;
    };
    parsed.codepoint == expected_cp && parsed.modifier == expected_mod
}

fn matches_printable_mok(data: &str, expected_cp: i32, expected_mod: u32) -> bool {
    if expected_mod == 0 {
        return false;
    }
    let Some(parsed) = parse_modify_other_keys(data) else {
        return false;
    };
    if parsed.modifier != expected_mod {
        return false;
    }
    normalize_shifted_letter(parsed.codepoint, parsed.modifier)
        == normalize_shifted_letter(expected_cp, expected_mod)
}

fn is_windows_terminal_session() -> bool {
    std::env::var_os("WT_SESSION").is_some()
        && std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("SSH_CLIENT").is_none()
        && std::env::var_os("SSH_TTY").is_none()
}

fn matches_raw_backspace(data: &str, expected_mod: u32) -> bool {
    if data == "\x7f" {
        return expected_mod == 0;
    }
    if data != "\x08" {
        return false;
    }
    if is_windows_terminal_session() {
        expected_mod == MOD_CTRL
    } else {
        expected_mod == 0
    }
}

fn raw_ctrl_char(key: &str) -> Option<char> {
    let ch = key.chars().next()?.to_ascii_lowercase();
    let code = ch as u32;
    if (97..=122).contains(&code) || matches!(ch, '[' | '\\' | ']' | '_') {
        return char::from_u32(code & 0x1f);
    }
    if ch == '-' {
        return char::from_u32(31);
    }
    None
}



struct ParsedKeyId {
    key: String,
    ctrl: bool,
    shift: bool,
    alt: bool,
    super_key: bool,
}

fn parse_key_id(key_id: &str) -> Option<ParsedKeyId> {
    let lower = key_id.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    let key = parts.last()?.to_string();
    if key.is_empty() {
        return None;
    }
    // pageup/pagedown normalize
    let key = match key.as_str() {
        "pageup" => "pageup".to_owned(),
        "pagedown" => "pagedown".to_owned(),
        other => other.to_owned(),
    };
    Some(ParsedKeyId {
        key,
        ctrl: parts.contains(&"ctrl"),
        shift: parts.contains(&"shift"),
        alt: parts.contains(&"alt"),
        super_key: parts.contains(&"super"),
    })
}

fn matches_legacy(data: &str, sequences: &[&str]) -> bool {
    sequences.contains(&data)
}

/// Check if input matches a key identifier (e.g. "ctrl+c", "escape").
pub fn matches_key(data: &str, key_id: &str) -> bool {
    let Some(parsed) = parse_key_id(key_id) else {
        return false;
    };
    let mut modifier = 0u32;
    if parsed.shift {
        modifier |= MOD_SHIFT;
    }
    if parsed.alt {
        modifier |= MOD_ALT;
    }
    if parsed.ctrl {
        modifier |= MOD_CTRL;
    }
    if parsed.super_key {
        modifier |= MOD_SUPER;
    }
    let kitty = is_kitty_protocol_active();
    let key = parsed.key.as_str();

    match key {
        "escape" | "esc" => {
            if modifier != 0 {
                return false;
            }
            data == "\x1b"
                || matches_kitty_sequence(data, CP_ESCAPE, 0)
                || matches_modify_other_keys(data, CP_ESCAPE, 0)
        }
        "space" => {
            if !kitty {
                if modifier == MOD_CTRL && data == "\x00" {
                    return true;
                }
                if modifier == MOD_ALT && data == "\x1b " {
                    return true;
                }
            }
            if modifier == 0 {
                data == " "
                    || matches_kitty_sequence(data, CP_SPACE, 0)
                    || matches_modify_other_keys(data, CP_SPACE, 0)
            } else {
                matches_kitty_sequence(data, CP_SPACE, modifier)
                    || matches_modify_other_keys(data, CP_SPACE, modifier)
            }
        }
        "tab" => {
            if modifier == MOD_SHIFT {
                data == "\x1b[Z"
                    || matches_kitty_sequence(data, CP_TAB, MOD_SHIFT)
                    || matches_modify_other_keys(data, CP_TAB, MOD_SHIFT)
            } else if modifier == 0 {
                data == "\t" || matches_kitty_sequence(data, CP_TAB, 0)
            } else {
                matches_kitty_sequence(data, CP_TAB, modifier)
                    || matches_modify_other_keys(data, CP_TAB, modifier)
            }
        }
        "enter" | "return" => match_enter(data, modifier, kitty),
        "backspace" => match_backspace(data, modifier),
        "insert" => match_functional(
            data,
            modifier,
            FN_INSERT,
            &["\x1b[2~"],
            Some(("\x1b[2$", "\x1b[2^")),
        ),
        "delete" => match_functional(
            data,
            modifier,
            FN_DELETE,
            &["\x1b[3~"],
            Some(("\x1b[3$", "\x1b[3^")),
        ),
        "clear" => {
            if modifier == 0 {
                matches_legacy(data, &["\x1b[E", "\x1bOE"])
            } else if modifier == MOD_SHIFT {
                data == "\x1b[e"
            } else if modifier == MOD_CTRL {
                data == "\x1bOe"
            } else {
                false
            }
        }
        "home" => match_functional(
            data,
            modifier,
            FN_HOME,
            &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
            Some(("\x1b[7$", "\x1b[7^")),
        ),
        "end" => match_functional(
            data,
            modifier,
            FN_END,
            &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
            Some(("\x1b[8$", "\x1b[8^")),
        ),
        "pageup" => match_functional(
            data,
            modifier,
            FN_PAGE_UP,
            &["\x1b[5~", "\x1b[[5~"],
            Some(("\x1b[5$", "\x1b[5^")),
        ),
        "pagedown" => match_functional(
            data,
            modifier,
            FN_PAGE_DOWN,
            &["\x1b[6~", "\x1b[[6~"],
            Some(("\x1b[6$", "\x1b[6^")),
        ),
        "up" => match_arrow(
            data,
            modifier,
            ARROW_UP,
            &["\x1b[A", "\x1bOA"],
            "\x1bp",
            "\x1b[a",
            "\x1bOa",
        ),
        "down" => match_arrow(
            data,
            modifier,
            ARROW_DOWN,
            &["\x1b[B", "\x1bOB"],
            "\x1bn",
            "\x1b[b",
            "\x1bOb",
        ),
        "left" => match_left_right(data, modifier, ARROW_LEFT, true, kitty),
        "right" => match_left_right(data, modifier, ARROW_RIGHT, false, kitty),
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            if modifier != 0 {
                return false;
            }
            let seqs = f_key_sequences(key);
            matches_legacy(data, seqs)
        }
        _ => match_printable(data, key, modifier, kitty),
    }
}

fn match_enter(data: &str, modifier: u32, kitty: bool) -> bool {
    if modifier == MOD_SHIFT {
        if matches_kitty_sequence(data, CP_ENTER, MOD_SHIFT)
            || matches_kitty_sequence(data, CP_KP_ENTER, MOD_SHIFT)
            || matches_modify_other_keys(data, CP_ENTER, MOD_SHIFT)
        {
            return true;
        }
        if kitty {
            return data == "\x1b\r" || data == "\n";
        }
        return false;
    }
    if modifier == MOD_ALT {
        if matches_kitty_sequence(data, CP_ENTER, MOD_ALT)
            || matches_kitty_sequence(data, CP_KP_ENTER, MOD_ALT)
            || matches_modify_other_keys(data, CP_ENTER, MOD_ALT)
        {
            return true;
        }
        if !kitty {
            return data == "\x1b\r";
        }
        return false;
    }
    if modifier == 0 {
        return data == "\r"
            || (!kitty && data == "\n")
            || data == "\x1bOM"
            || matches_kitty_sequence(data, CP_ENTER, 0)
            || matches_kitty_sequence(data, CP_KP_ENTER, 0);
    }
    matches_kitty_sequence(data, CP_ENTER, modifier)
        || matches_kitty_sequence(data, CP_KP_ENTER, modifier)
        || matches_modify_other_keys(data, CP_ENTER, modifier)
}

fn match_backspace(data: &str, modifier: u32) -> bool {
    if modifier == MOD_ALT {
        if data == "\x1b\x7f" || data == "\x1b\x08" {
            return true;
        }
        return matches_kitty_sequence(data, CP_BACKSPACE, MOD_ALT)
            || matches_modify_other_keys(data, CP_BACKSPACE, MOD_ALT);
    }
    if modifier == MOD_CTRL {
        if matches_raw_backspace(data, MOD_CTRL) {
            return true;
        }
        return matches_kitty_sequence(data, CP_BACKSPACE, MOD_CTRL)
            || matches_modify_other_keys(data, CP_BACKSPACE, MOD_CTRL);
    }
    if modifier == 0 {
        return matches_raw_backspace(data, 0)
            || matches_kitty_sequence(data, CP_BACKSPACE, 0)
            || matches_modify_other_keys(data, CP_BACKSPACE, 0);
    }
    matches_kitty_sequence(data, CP_BACKSPACE, modifier)
        || matches_modify_other_keys(data, CP_BACKSPACE, modifier)
}

fn match_functional(
    data: &str,
    modifier: u32,
    cp: i32,
    legacy: &[&str],
    shift_ctrl: Option<(&str, &str)>,
) -> bool {
    if modifier == 0 {
        return matches_legacy(data, legacy) || matches_kitty_sequence(data, cp, 0);
    }
    if let Some((shift_seq, ctrl_seq)) = shift_ctrl {
        if modifier == MOD_SHIFT && data == shift_seq {
            return true;
        }
        if modifier == MOD_CTRL && data == ctrl_seq {
            return true;
        }
    }
    matches_kitty_sequence(data, cp, modifier)
}

fn match_arrow(
    data: &str,
    modifier: u32,
    cp: i32,
    legacy: &[&str],
    alt_seq: &str,
    shift_seq: &str,
    ctrl_seq: &str,
) -> bool {
    if modifier == MOD_ALT {
        return data == alt_seq || matches_kitty_sequence(data, cp, MOD_ALT);
    }
    if modifier == 0 {
        return matches_legacy(data, legacy) || matches_kitty_sequence(data, cp, 0);
    }
    if modifier == MOD_SHIFT && data == shift_seq {
        return true;
    }
    if modifier == MOD_CTRL && data == ctrl_seq {
        return true;
    }
    matches_kitty_sequence(data, cp, modifier)
}

fn match_left_right(data: &str, modifier: u32, cp: i32, is_left: bool, kitty: bool) -> bool {
    let (csi_alt, legacy_alt, csi_ctrl, shift_seq, ctrl_seq, legacy) = if is_left {
        (
            "\x1b[1;3D",
            "\x1bB",
            "\x1b[1;5D",
            "\x1b[d",
            "\x1bOd",
            &["\x1b[D", "\x1bOD"][..],
        )
    } else {
        (
            "\x1b[1;3C",
            "\x1bF",
            "\x1b[1;5C",
            "\x1b[c",
            "\x1bOc",
            &["\x1b[C", "\x1bOC"][..],
        )
    };
    let alt_letter = if is_left { "\x1bb" } else { "\x1bf" };
    if modifier == MOD_ALT {
        return data == csi_alt
            || (!kitty && data == legacy_alt)
            || data == alt_letter
            || matches_kitty_sequence(data, cp, MOD_ALT);
    }
    if modifier == MOD_CTRL {
        return data == csi_ctrl || data == ctrl_seq || matches_kitty_sequence(data, cp, MOD_CTRL);
    }
    if modifier == 0 {
        return matches_legacy(data, legacy) || matches_kitty_sequence(data, cp, 0);
    }
    if modifier == MOD_SHIFT && data == shift_seq {
        return true;
    }
    matches_kitty_sequence(data, cp, modifier)
}

fn f_key_sequences(key: &str) -> &'static [&'static str] {
    match key {
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => &[],
    }
}

fn match_printable(data: &str, key: &str, modifier: u32, kitty: bool) -> bool {
    if key.len() != 1 {
        return false;
    }
    let ch = key.chars().next().unwrap();
    let is_letter = ch.is_ascii_lowercase();
    let is_digit = ch.is_ascii_digit();
    if !(is_letter || is_digit || is_symbol_key(key)) {
        return false;
    }
    let codepoint = ch as i32;
    let raw_ctrl = raw_ctrl_char(key);

    if modifier == MOD_CTRL + MOD_ALT && !kitty
        && let Some(rc) = raw_ctrl
            && data == format!("\x1b{rc}") {
                return true;
            }
    if modifier == MOD_ALT && !kitty && (is_letter || is_digit || is_symbol_key(key))
        && data == format!("\x1b{key}") {
            return true;
        }
    if modifier == MOD_CTRL {
        if let Some(rc) = raw_ctrl
            && data == rc.to_string() {
                return true;
            }
        return matches_kitty_sequence(data, codepoint, MOD_CTRL)
            || matches_printable_mok(data, codepoint, MOD_CTRL);
    }
    if modifier == MOD_SHIFT + MOD_CTRL {
        return matches_kitty_sequence(data, codepoint, MOD_SHIFT + MOD_CTRL)
            || matches_printable_mok(data, codepoint, MOD_SHIFT + MOD_CTRL);
    }
    if modifier == MOD_SHIFT {
        if is_letter && data == key.to_ascii_uppercase() {
            return true;
        }
        return matches_kitty_sequence(data, codepoint, MOD_SHIFT)
            || matches_printable_mok(data, codepoint, MOD_SHIFT);
    }
    if modifier != 0 {
        return matches_kitty_sequence(data, codepoint, modifier)
            || matches_printable_mok(data, codepoint, modifier);
    }
    data == key || matches_kitty_sequence(data, codepoint, 0)
}

/// Is this a Kitty key-release event?
pub fn is_key_release(data: &str) -> bool {
    if data.contains("\x1b[200~") {
        return false;
    }
    data.contains(":3u")
        || data.contains(":3~")
        || data.contains(":3A")
        || data.contains(":3B")
        || data.contains(":3C")
        || data.contains(":3D")
        || data.contains(":3H")
        || data.contains(":3F")
}

pub fn is_key_repeat(data: &str) -> bool {
    if data.contains("\x1b[200~") {
        return false;
    }
    data.contains(":2u")
        || data.contains(":2~")
        || data.contains(":2A")
        || data.contains(":2B")
        || data.contains(":2C")
        || data.contains(":2D")
        || data.contains(":2H")
        || data.contains(":2F")
}

/// Parse input to a key id string (e.g. "ctrl+c").
pub fn parse_key(data: &str) -> Option<String> {
    if let Some(kitty) = parse_kitty_sequence(data) {
        return format_parsed_key(kitty.codepoint, kitty.modifier, kitty.base_layout_key);
    }
    if let Some(mok) = parse_modify_other_keys(data) {
        return format_parsed_key(mok.codepoint, mok.modifier, None);
    }
    let kitty_active = is_kitty_protocol_active();
    if kitty_active && (data == "\x1b\r" || data == "\n") {
        return Some("shift+enter".into());
    }
    if let Some(id) = LEGACY_SEQUENCE_IDS.get(data) {
        return Some((*id).to_owned());
    }
    match data {
        "\x1b" => return Some("escape".into()),
        "\x1c" => return Some("ctrl+\\".into()),
        "\x1d" => return Some("ctrl+]".into()),
        "\x1f" => return Some("ctrl+-".into()),
        "\x1b\x1b" => return Some("ctrl+alt+[".into()),
        "\x1b\x1c" => return Some("ctrl+alt+\\".into()),
        "\x1b\x1d" => return Some("ctrl+alt+]".into()),
        "\x1b\x1f" => return Some("ctrl+alt+-".into()),
        "\t" => return Some("tab".into()),
        "\r" | "\x1bOM" => return Some("enter".into()),
        "\x00" => return Some("ctrl+space".into()),
        " " => return Some("space".into()),
        "\x7f" => return Some("backspace".into()),
        "\x1b[Z" => return Some("shift+tab".into()),
        "\x1b\x7f" | "\x1b\x08" => return Some("alt+backspace".into()),
        "\x1b[A" => return Some("up".into()),
        "\x1b[B" => return Some("down".into()),
        "\x1b[C" => return Some("right".into()),
        "\x1b[D" => return Some("left".into()),
        "\x1b[H" | "\x1bOH" => return Some("home".into()),
        "\x1b[F" | "\x1bOF" => return Some("end".into()),
        "\x1b[3~" => return Some("delete".into()),
        "\x1b[5~" => return Some("pageUp".into()),
        "\x1b[6~" => return Some("pageDown".into()),
        _ => {}
    }
    if !kitty_active && data == "\n" {
        return Some("enter".into());
    }
    if data == "\x08" {
        return Some(if is_windows_terminal_session() {
            "ctrl+backspace".into()
        } else {
            "backspace".into()
        });
    }
    if !kitty_active && data == "\x1b\r" {
        return Some("alt+enter".into());
    }
    if !kitty_active && data == "\x1b " {
        return Some("alt+space".into());
    }
    if !kitty_active && data == "\x1bB" {
        return Some("alt+left".into());
    }
    if !kitty_active && data == "\x1bF" {
        return Some("alt+right".into());
    }
    if !kitty_active && data.len() == 2 && data.as_bytes()[0] == 0x1b {
        let code = data.as_bytes()[1] as u32;
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+alt+{}", char::from_u32(code + 96)?));
        }
        let key = char::from_u32(code)?;
        if key.is_ascii_lowercase() || key.is_ascii_digit() || is_symbol_key(&key.to_string()) {
            return Some(format!("alt+{key}"));
        }
    }
    if data.len() == 1 {
        let code = data.as_bytes()[0] as u32;
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+{}", char::from_u32(code + 96)?));
        }
        if (32..=126).contains(&code) {
            return Some(data.to_owned());
        }
    }
    None
}

fn format_parsed_key(
    codepoint: i32,
    modifier: u32,
    base_layout_key: Option<i32>,
) -> Option<String> {
    let normalized = normalize_kitty_functional(codepoint);
    let identity = normalize_shifted_letter(normalized, modifier);
    let is_latin = (97..=122).contains(&identity);
    let is_digit = (48..=57).contains(&identity);
    let is_symbol = char::from_u32(identity as u32)
        .map(|c| is_symbol_key(&c.to_string()))
        .unwrap_or(false);
    let effective = if is_latin || is_digit || is_symbol {
        identity
    } else {
        base_layout_key.unwrap_or(identity)
    };
    let key_name = match effective {
        CP_ESCAPE => "escape",
        CP_TAB => "tab",
        CP_ENTER | CP_KP_ENTER => "enter",
        CP_SPACE => "space",
        CP_BACKSPACE => "backspace",
        FN_DELETE => "delete",
        FN_INSERT => "insert",
        FN_HOME => "home",
        FN_END => "end",
        FN_PAGE_UP => "pageUp",
        FN_PAGE_DOWN => "pageDown",
        ARROW_UP => "up",
        ARROW_DOWN => "down",
        ARROW_LEFT => "left",
        ARROW_RIGHT => "right",
        48..=57 | 97..=122 => {
            return format_key_name_with_modifiers(
                &char::from_u32(effective as u32)?.to_string(),
                modifier,
            );
        }
        other => {
            let ch = char::from_u32(other as u32)?;
            if is_symbol_key(&ch.to_string()) {
                return format_key_name_with_modifiers(&ch.to_string(), modifier);
            }
            return None;
        }
    };
    format_key_name_with_modifiers(key_name, modifier)
}

fn format_key_name_with_modifiers(key_name: &str, modifier: u32) -> Option<String> {
    let effective = modifier & !LOCK_MASK;
    let supported = MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_SUPER;
    if (effective & !supported) != 0 {
        return None;
    }
    let mut mods = Vec::new();
    if effective & MOD_SHIFT != 0 {
        mods.push("shift");
    }
    if effective & MOD_CTRL != 0 {
        mods.push("ctrl");
    }
    if effective & MOD_ALT != 0 {
        mods.push("alt");
    }
    if effective & MOD_SUPER != 0 {
        mods.push("super");
    }
    if mods.is_empty() {
        Some(key_name.to_owned())
    } else {
        Some(format!("{}+{key_name}", mods.join("+")))
    }
}

static LEGACY_SEQUENCE_IDS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for (k, v) in [
        ("\x1bOA", "up"),
        ("\x1bOB", "down"),
        ("\x1bOC", "right"),
        ("\x1bOD", "left"),
        ("\x1bOH", "home"),
        ("\x1bOF", "end"),
        ("\x1b[E", "clear"),
        ("\x1bOE", "clear"),
        ("\x1bOe", "ctrl+clear"),
        ("\x1b[e", "shift+clear"),
        ("\x1b[2~", "insert"),
        ("\x1b[2$", "shift+insert"),
        ("\x1b[2^", "ctrl+insert"),
        ("\x1b[3$", "shift+delete"),
        ("\x1b[3^", "ctrl+delete"),
        ("\x1b[[5~", "pageUp"),
        ("\x1b[[6~", "pageDown"),
        ("\x1b[a", "shift+up"),
        ("\x1b[b", "shift+down"),
        ("\x1b[c", "shift+right"),
        ("\x1b[d", "shift+left"),
        ("\x1bOa", "ctrl+up"),
        ("\x1bOb", "ctrl+down"),
        ("\x1bOc", "ctrl+right"),
        ("\x1bOd", "ctrl+left"),
        ("\x1b[5$", "shift+pageUp"),
        ("\x1b[6$", "shift+pageDown"),
        ("\x1b[7$", "shift+home"),
        ("\x1b[8$", "shift+end"),
        ("\x1b[5^", "ctrl+pageUp"),
        ("\x1b[6^", "ctrl+pageDown"),
        ("\x1b[7^", "ctrl+home"),
        ("\x1b[8^", "ctrl+end"),
        ("\x1bOP", "f1"),
        ("\x1bOQ", "f2"),
        ("\x1bOR", "f3"),
        ("\x1bOS", "f4"),
        ("\x1b[11~", "f1"),
        ("\x1b[12~", "f2"),
        ("\x1b[13~", "f3"),
        ("\x1b[14~", "f4"),
        ("\x1b[[A", "f1"),
        ("\x1b[[B", "f2"),
        ("\x1b[[C", "f3"),
        ("\x1b[[D", "f4"),
        ("\x1b[[E", "f5"),
        ("\x1b[15~", "f5"),
        ("\x1b[17~", "f6"),
        ("\x1b[18~", "f7"),
        ("\x1b[19~", "f8"),
        ("\x1b[20~", "f9"),
        ("\x1b[21~", "f10"),
        ("\x1b[23~", "f11"),
        ("\x1b[24~", "f12"),
        ("\x1bb", "alt+left"),
        ("\x1bf", "alt+right"),
        ("\x1bp", "alt+up"),
        ("\x1bn", "alt+down"),
    ] {
        m.insert(k, v);
    }
    m
});

/// Decode Kitty CSI-u / modifyOtherKeys to a printable char.
pub fn decode_printable_key(data: &str) -> Option<String> {
    decode_kitty_printable(data).or_else(|| decode_mok_printable(data))
}

pub fn decode_kitty_printable(data: &str) -> Option<String> {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^\x1b\[(\d+)(?::(\d*))?(?::(\d+))?(?:;(\d+))?(?::(\d+))?u$").unwrap()
    });
    let caps = RE.captures(data)?;
    let codepoint: i32 = caps[1].parse().ok()?;
    let shifted = caps
        .get(2)
        .filter(|m| !m.as_str().is_empty())
        .and_then(|m| m.as_str().parse::<i32>().ok());
    let mod_value: u32 = caps
        .get(4)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(1);
    let modifier = mod_value.saturating_sub(1);
    let allowed = MOD_SHIFT | LOCK_MASK;
    if (modifier & !allowed) != 0 {
        return None;
    }
    if modifier & (MOD_ALT | MOD_CTRL) != 0 {
        return None;
    }
    let mut effective = codepoint;
    if modifier & MOD_SHIFT != 0
        && let Some(s) = shifted {
            effective = s;
        }
    effective = normalize_kitty_functional(effective);
    if effective < 32 {
        return None;
    }
    char::from_u32(effective as u32).map(|c| c.to_string())
}

fn decode_mok_printable(data: &str) -> Option<String> {
    let parsed = parse_modify_other_keys(data)?;
    let modifier = parsed.modifier & !LOCK_MASK;
    if (modifier & !MOD_SHIFT) != 0 {
        return None;
    }
    if parsed.codepoint < 32 {
        return None;
    }
    char::from_u32(parsed.codepoint as u32).map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn kitty_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn enter_and_ctrl_c() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(false);
        assert!(matches_key("\r", "enter"));
        assert!(matches_key("\x03", "ctrl+c"));
        assert!(matches_key("\x1b[A", "up"));
        assert!(matches_key("\x1b[Z", "shift+tab"));
    }

    #[test]
    fn shift_enter_kitty_mode() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(true);
        assert!(matches_key("\x1b\r", "shift+enter"));
        assert!(matches_key("\n", "shift+enter"));
        assert!(!matches_key("\x1b\r", "alt+enter"));
        set_kitty_protocol_active(false);
        assert!(matches_key("\x1b\r", "alt+enter"));
    }

    #[test]
    fn parse_key_basic() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(false);
        assert_eq!(parse_key("\r").as_deref(), Some("enter"));
        assert_eq!(parse_key("\x03").as_deref(), Some("ctrl+c"));
    }

    #[test]
    fn kitty_base_layout_non_latin() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(true);
        // Cyrillic 'с' + base Latin 'c' with ctrl
        assert!(matches_key("\x1b[1089::99;5u", "ctrl+c"));
        assert!(matches_key("\x1b[1074::100;5u", "ctrl+d"));
        assert!(matches_key("\x1b[1103::122;5u", "ctrl+z"));
        // ctrl+shift+p via base layout
        assert!(matches_key("\x1b[1079::112;6u", "ctrl+shift+p"));
        // Latin without base still matches
        assert!(matches_key("\x1b[99;5u", "ctrl+c"));
        // Wrong key / wrong modifiers
        assert!(!matches_key("\x1b[1089::99;5u", "ctrl+d"));
        assert!(!matches_key("\x1b[1089::99;5u", "ctrl+shift+c"));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn kitty_super_and_combined_modifiers() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(true);
        assert!(matches_key("\x1b[107;9u", "super+k"));
        assert!(matches_key("\x1b[13;9u", "super+enter"));
        assert!(matches_key("\x1b[107;13u", "ctrl+super+k"));
        assert!(matches_key("\x1b[107;14u", "ctrl+shift+super+k"));
        assert!(!matches_key("\x1b[107;13u", "super+k"));
        assert_eq!(parse_key("\x1b[107;9u").as_deref(), Some("super+k"));
        assert_eq!(parse_key("\x1b[13;9u").as_deref(), Some("super+enter"));
        assert_eq!(parse_key("\x1b[107;13u").as_deref(), Some("ctrl+super+k"));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn kitty_digits_and_keypad_functional() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(true);
        assert!(matches_key("\x1b[49u", "1"));
        assert!(matches_key("\x1b[49;5u", "ctrl+1"));
        assert!(!matches_key("\x1b[49;5u", "ctrl+2"));
        assert_eq!(parse_key("\x1b[49u").as_deref(), Some("1"));
        assert_eq!(parse_key("\x1b[49;5u").as_deref(), Some("ctrl+1"));

        assert!(matches_key("\x1b[57400u", "1"));
        assert!(matches_key("\x1b[57410u", "/"));
        assert!(matches_key("\x1b[57417u", "left"));
        assert!(matches_key("\x1b[57426u", "delete"));
        assert_eq!(parse_key("\x1b[57399u").as_deref(), Some("0"));
        assert_eq!(parse_key("\x1b[57409u").as_deref(), Some("."));
        assert_eq!(parse_key("\x1b[57417u").as_deref(), Some("left"));
        assert_eq!(parse_key("\x1b[57426u").as_deref(), Some("delete"));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn kitty_shifted_and_event_type_formats() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(true);
        assert!(matches_key("\x1b[99:67:99;2u", "shift+c"));
        assert!(matches_key("\x1b[1089::99;5:3u", "ctrl+c")); // release still matches
        assert!(matches_key("\x1b[1089:1057:99;6:2u", "ctrl+shift+c"));
        // Prefer codepoint over base for Latin / symbols
        assert!(matches_key("\x1b[107::118;5u", "ctrl+k"));
        assert!(!matches_key("\x1b[107::118;5u", "ctrl+v"));
        assert!(matches_key("\x1b[47::91;5u", "ctrl+/"));
        assert!(!matches_key("\x1b[47::91;5u", "ctrl+["));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn modify_other_keys_matching() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(false);
        assert!(matches_key("\x1b[27;5;99~", "ctrl+c"));
        assert_eq!(decode_printable_key("\x1b[27;2;69~").as_deref(), Some("E"));
        assert_eq!(decode_printable_key("\x1b[27;2;196~").as_deref(), Some("Ä"));
        assert_eq!(decode_printable_key("\x1b[27;2;32~").as_deref(), Some(" "));
        assert_eq!(decode_printable_key("\x1b[27;2;13~"), None);
    }

    #[test]
    fn decode_kitty_printable_keypad() {
        let _g = kitty_test_lock();
        assert_eq!(decode_kitty_printable("\x1b[57399u").as_deref(), Some("0"));
        assert_eq!(decode_kitty_printable("\x1b[57400u").as_deref(), Some("1"));
        assert_eq!(decode_kitty_printable("\x1b[57409u").as_deref(), Some("."));
        assert_eq!(decode_kitty_printable("\x1b[57410u").as_deref(), Some("/"));
        assert_eq!(decode_kitty_printable("\x1b[57411u").as_deref(), Some("*"));
        assert_eq!(decode_kitty_printable("\x1b[57412u").as_deref(), Some("-"));
        assert_eq!(decode_kitty_printable("\x1b[57413u").as_deref(), Some("+"));
        assert_eq!(decode_kitty_printable("\x1b[57415u").as_deref(), Some("="));
        assert_eq!(decode_kitty_printable("\x1b[57416u").as_deref(), Some(","));
        // left arrow functional — not printable
        assert_eq!(decode_kitty_printable("\x1b[57417u"), None);
    }

    #[test]
    fn enter_mode_dependent_legacy_vs_kitty() {
        let _g = kitty_test_lock();
        set_kitty_protocol_active(false);
        assert!(matches_key("\r", "enter"));
        // Legacy: \x1b\r is alt+enter
        assert!(matches_key("\x1b\r", "alt+enter"));
        set_kitty_protocol_active(true);
        // Kitty: \n and \x1b\r are shift+enter
        assert!(matches_key("\n", "shift+enter"));
        assert!(matches_key("\x1b\r", "shift+enter"));
        assert!(!matches_key("\x1b\r", "alt+enter"));
        set_kitty_protocol_active(false);
    }
}
