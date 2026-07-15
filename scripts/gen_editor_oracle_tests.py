#!/usr/bin/env python3
"""Generate behavior-faithful Rust editor oracle tests from editor.test.ts.

Expected values come only from the TypeScript asserts (never from Rust output).
"""
from __future__ import annotations

import json
import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ORACLE = Path(
    "/home/alpha/exp/pi-rust/.references/pi/packages/tui/test/editor.test.ts"
)
OUT_DIR = ROOT / "crates/pi-tui/tests"

HELPERS = r'''
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
'''


def rust_ident(name: str) -> str:
    s = re.sub(r"[^a-zA-Z0-9]+", "_", name).strip("_").lower()
    if not s or s[0].isdigit():
        s = "case_" + s
    return s[:90]


def js_str_to_rust(s: str) -> str:
    out = ['"']
    for ch in s:
        o = ord(ch)
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\0":
            out.append("\\0")
        elif ch == "\x7f":
            out.append("\\x7f")
        elif o < 0x20:
            out.append(f"\\x{o:02x}")
        elif o > 0x7F:
            out.append(f"\\u{{{o:x}}}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def extract_js_string(src: str, start: int):
    q = src[start]
    if q not in "\"'":
        return None
    i = start + 1
    chars: list[str] = []
    while i < len(src):
        c = src[i]
        if c == "\\":
            i += 1
            if i >= len(src):
                break
            e = src[i]
            if e == "n":
                chars.append("\n")
            elif e == "r":
                chars.append("\r")
            elif e == "t":
                chars.append("\t")
            elif e == "\\":
                chars.append("\\")
            elif e == '"':
                chars.append('"')
            elif e == "'":
                chars.append("'")
            elif e == "0":
                chars.append("\0")
            elif e == "x" and i + 2 < len(src):
                chars.append(chr(int(src[i + 1 : i + 3], 16)))
                i += 2
            elif e == "u" and i + 1 < len(src) and src[i + 1] == "{":
                j = src.find("}", i + 2)
                chars.append(chr(int(src[i + 2 : j], 16)))
                i = j
            elif e == "u" and i + 4 < len(src):
                chars.append(chr(int(src[i + 1 : i + 5], 16)))
                i += 4
            else:
                chars.append(e)
            i += 1
            continue
        if c == q:
            return ("".join(chars), i + 1)
        chars.append(c)
        i += 1
    return None


def eval_js_arg(arg: str):
    arg = arg.strip().rstrip(",")
    if (arg.startswith('"') and arg.endswith('"')) or (
        arg.startswith("'") and arg.endswith("'")
    ):
        res = extract_js_string(arg, 0)
        if res:
            return res[0]
    if arg.startswith("`") and arg.endswith("`") and "${" not in arg:
        inner = arg[1:-1]
        # unescape lightly
        return (
            inner.encode("utf-8")
            .decode("unicode_escape")
            if "\\" in inner
            else inner
        )
    return None


def extract_cases(text: str):
    pattern = re.compile(
        r'it\(("(?:\\.|[^"\\])*")\s*,\s*(async\s*)?\(\)\s*=>\s*\{',
        re.M,
    )
    cases = []
    for m in pattern.finditer(text):
        name = json.loads(m.group(1))
        start = m.end()
        i = start
        depth = 1
        while i < len(text) and depth:
            c = text[i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
            elif c in "\"'":
                q = c
                i += 1
                while i < len(text):
                    if text[i] == "\\":
                        i += 2
                        continue
                    if text[i] == q:
                        break
                    i += 1
            elif c == "`":
                i += 1
                while i < len(text) and text[i] != "`":
                    if text[i] == "\\":
                        i += 2
                        continue
                    i += 1
            i += 1
        body = text[start : i - 1]
        cases.append((name, bool(m.group(2)), body, m.start()))
    return cases


def categorize(text: str, cases):
    descs = []
    for m in re.finditer(r'describe\("([^"]+)"', text):
        descs.append((text[: m.start()].count("\n") + 1, m.group(1)))
    by = defaultdict(list)
    for name, is_async, body, pos in cases:
        line = text[:pos].count("\n") + 1
        cur = "root"
        for ln, dname in descs:
            if ln <= line:
                cur = dname
            else:
                break
        by[cur].append((name, is_async, body))
    return by


def translate_simple(body: str) -> str | None:
    """Translate simple sequential editor tests. None if too complex."""
    if "mockProvider" in body or "AutocompleteProvider" in body:
        return None
    if "as unknown as" in body:
        return None
    if "for (const " in body or "for (let i = 0; i <" in body and "addToHistory" in body:
        # allow some for-loops below
        pass
    if "Intl.SegmentData" in body or "preSegmented" in body:
        return None
    if "visibleWidth" in body and "render(" in body:
        # wrapping visual tests often need special handling
        if "contentLines" in body or "stripVTControlCharacters" in body:
            return None

    pure_wrap = "wordWrapLine(" in body and "new Editor" not in body
    if pure_wrap:
        return translate_wrap_only(body)

    lines: list[str] = []
    m = re.search(r"createTestTUI\((\d+)\s*,\s*(\d+)\)", body)
    m2 = re.search(r"createTestTUI\((\d+)\)", body)
    if m:
        lines.append(f"let t = tui_size({m.group(1)}, {m.group(2)});")
    elif m2:
        lines.append(f"let t = tui_size({m2.group(1)}, 24);")
    else:
        lines.append("let t = tui();")

    if re.search(r"paddingX\s*:", body):
        pm = re.search(r"paddingX\s*:\s*(\d+)", body)
        if not pm:
            return None
        lines.append(f"let mut e = editor_opts(&t, {pm.group(1)});")
    else:
        lines.append("let mut e = editor(&t);")

    if "onSubmit" in body:
        lines.append(
            "let submitted = std::rc::Rc::new(std::cell::RefCell::new(String::new()));"
        )
        lines.append("let s2 = submitted.clone();")
        lines.append(
            "e.on_submit = Some(Box::new(move |text| { *s2.borrow_mut() = text; }));"
        )
        # also bool form
        if "submitted = true" in body or "submitted = false" in body or "let submitted = false" in body:
            lines = [l for l in lines if "submitted" not in l or l.startswith("let t")]
            # rebuild
            lines = [l for l in lines if "on_submit" not in l and "s2" not in l and "submitted" not in l]
            lines.append(
                "let submitted = std::rc::Rc::new(std::cell::RefCell::new(false));"
            )
            lines.append("let s2 = submitted.clone();")
            lines.append(
                "e.on_submit = Some(Box::new(move |_| { *s2.borrow_mut() = true; }));"
            )

    # Strip line comments for matching
    cleaned = re.sub(r"//[^\n]*", "", body)
    # Normalize whitespace for some patterns
    # Process common call patterns in order of appearance
    pos = 0
    while pos < len(cleaned):
        # skip whitespace
        while pos < len(cleaned) and cleaned[pos] in " \t\n\r":
            pos += 1
        if pos >= len(cleaned):
            break
        rest = cleaned[pos:]

        # for (let i = 0; i < N; i++) editor.handleInput("...");
        m = re.match(
            r"for\s*\(\s*let\s+\w+\s*=\s*0;\s*\w+\s*<\s*(\d+)\s*;\s*\w+\+\+\s*\)\s*"
            r"editor\.handleInput\((\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(2))
            if s is None:
                return None
            lines.append(
                f"for _ in 0..{m.group(1)} {{ e.handle_input({js_str_to_rust(s)}); }}"
            )
            pos += m.end()
            continue

        # for (let i = 0; i < N; i++) { editor.addToHistory(`prompt ${i}`); }
        m = re.match(
            r"for\s*\(\s*let\s+(\w+)\s*=\s*0;\s*\1\s*<\s*(\d+)\s*;\s*\1\+\+\s*\)\s*\{\s*"
            r"editor\.addToHistory\(`prompt \$\{(\1)\}`\)\s*;\s*\}",
            rest,
        )
        if m:
            lines.append(
                f"for i in 0..{m.group(2)} {{ e.add_to_history(&format!(\"prompt {{i}}\")); }}"
            )
            pos += m.end()
            continue

        # for (const ch of "...") editor.handleInput(ch);
        m = re.match(
            r"for\s*\(\s*const\s+\w+\s+of\s+(\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\s*\)\s*"
            r"editor\.handleInput\(\w+\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(
                f"for ch in {js_str_to_rust(s)}.chars() {{ e.handle_input(&ch.to_string()); }}"
            )
            pos += m.end()
            continue

        m = re.match(
            r"editor\.addToHistory\((\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(f"e.add_to_history({js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(
            r"editor\.setText\((\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(f"e.set_text({js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(
            r"editor\.handleInput\((\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(f"e.handle_input({js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(
            r"editor\.insertTextAtCursor\((\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(f"e.insert_text_at_cursor({js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(r"editor\.render\((\d+)\)\s*;?", rest)
        if m:
            lines.append(f"let _ = e.render({m.group(1)});")
            pos += m.end()
            continue

        m = re.match(r"positionCursor\(editor,\s*(\d+),\s*(\d+)\)\s*;?", rest)
        if m:
            lines.append(f"position_cursor(&mut e, {m.group(1)}, {m.group(2)});")
            pos += m.end()
            continue

        m = re.match(r"(?:const\s+\w+\s*=\s*)?pasteWithMarker\(editor\)\s*;?", rest)
        if m:
            if rest.lstrip().startswith("const"):
                lines.append("let text = paste_with_marker(&mut e);")
            else:
                lines.append("let _ = paste_with_marker(&mut e);")
            pos += m.end()
            continue

        m = re.match(r"const\s+text\s*=\s*editor\.getText\(\)\s*;?", rest)
        if m:
            lines.append("let text = e.get_text();")
            pos += m.end()
            continue

        m = re.match(r"const\s+textBefore\s*=\s*editor\.getText\(\)\s*;?", rest)
        if m:
            lines.append("let text_before = e.get_text();")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.strictEqual\(editor\.getText\(\),\s*(\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(f"assert_eq!(e.get_text(), {js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.strictEqual\(editor\.getText\(\),\s*textBefore\)\s*;?",
            rest,
        )
        if m:
            lines.append("assert_eq!(e.get_text(), text_before);")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.strictEqual\(editor\.getExpandedText\(\),\s*(\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|pastedText)\)\s*;?",
            rest,
        )
        if m:
            if m.group(1) == "pastedText":
                lines.append("assert_eq!(e.get_expanded_text(), pasted_text);")
            else:
                s = eval_js_arg(m.group(1))
                if s is None:
                    return None
                lines.append(f"assert_eq!(e.get_expanded_text(), {js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.deepStrictEqual\(editor\.getCursor\(\),\s*\{\s*line:\s*(\d+)\s*,\s*col:\s*(\d+)\s*\}\)\s*;?",
            rest,
        )
        if m:
            lines.append(f"assert_eq!(e.get_cursor(), ({m.group(1)}, {m.group(2)}));")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.strictEqual\(editor\.getCursor\(\)\.line,\s*(\d+)\)\s*;?",
            rest,
        )
        if m:
            lines.append(f"assert_eq!(e.get_cursor().0, {m.group(1)});")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.strictEqual\(editor\.getCursor\(\)\.col,\s*(\d+)\)\s*;?",
            rest,
        )
        if m:
            lines.append(f"assert_eq!(e.get_cursor().1, {m.group(1)});")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.equal\(editor\.getCursor\(\)\.col,\s*(\d+)\)\s*;?",
            rest,
        )
        if m:
            lines.append(f"assert_eq!(e.get_cursor().1, {m.group(1)});")
            pos += m.end()
            continue

        m = re.match(r"assert\.strictEqual\(submitted,\s*(true|false)\)\s*;?", rest)
        if m:
            lines.append(f"assert_eq!(*submitted.borrow(), {m.group(1)});")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.strictEqual\(submitted,\s*(\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|pastedText)\)\s*;?",
            rest,
        )
        if m:
            if m.group(1) == "pastedText":
                lines.append("assert_eq!(*submitted.borrow(), pasted_text);")
            else:
                s = eval_js_arg(m.group(1))
                if s is None:
                    return None
                lines.append(f"assert_eq!(*submitted.borrow(), {js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(
            r"assert\.strictEqual\(text,\s*(\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(f"assert_eq!(text, {js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(
            r"const\s+text\s*=\s*editor\.getText\(\)\s*;\s*assert\.strictEqual\(text,\s*(\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)\s*;?",
            rest,
        )
        if m:
            s = eval_js_arg(m.group(1))
            if s is None:
                return None
            lines.append(f"assert_eq!(e.get_text(), {js_str_to_rust(s)});")
            pos += m.end()
            continue

        m = re.match(r"assert\.match\((?:text|editor\.getText\(\)),\s*/(.+)/\s*\)\s*;?", rest)
        if m:
            # convert simple regex
            pat = m.group(1).replace("\\\\", "\\")
            lines.append(
                f'assert!(regex::Regex::new(r"{pat}").unwrap().is_match(&e.get_text()));'
            )
            pos += m.end()
            continue

        # skip pure declarations we handle elsewhere
        m = re.match(r"let\s+submitted\s*=\s*false\s*;?", rest)
        if m:
            pos += m.end()
            continue
        m = re.match(r"editor\.onSubmit\s*=\s*\([^)]*\)\s*=>\s*\{[^}]*\}\s*;?", rest)
        if m:
            pos += m.end()
            continue
        m = re.match(r"const\s+editor\s*=\s*new Editor\([^;]+;?", rest)
        if m:
            # already handled
            # find end of statement
            semi = rest.find(";")
            if semi < 0:
                return None
            pos += semi + 1
            continue
        m = re.match(r"const\s+tui\s*=\s*createTestTUI\([^;]+;?", rest)
        if m:
            semi = rest.find(";")
            pos += semi + 1
            continue

        # Skip assert.ok with functions etc - fail
        if rest.startswith("assert.") or rest.startswith("editor.") or rest.startswith("const ") or rest.startswith("let ") or rest.startswith("for "):
            return None

        # unknown token - skip one char? better fail
        return None

    if len(lines) < 3:
        return None
    return "\n".join("    " + l for l in lines)


def translate_wrap_only(body: str) -> str | None:
    if "segments" in body:
        return None
    # const line = ` ${"a".repeat(186)}你`;
    line_val = None
    lm = re.search(r"const line = `([^`]*)`", body)
    if lm:
        tmpl = lm.group(1)

        def repl(mm):
            expr = mm.group(1)
            rm = re.match(r'"([^"]*)"\.repeat\((\d+)\)', expr)
            if rm:
                return rm.group(1) * int(rm.group(2))
            raise ValueError(expr)

        try:
            line_val = re.sub(r"\$\{([^}]+)\}", lambda mm: repl(mm), tmpl)
        except Exception:
            return None

    m = re.search(r"wordWrapLine\((.+?),\s*(\d+)\)", body)
    if not m:
        return None
    arg = m.group(1).strip()
    width = m.group(2)
    if arg == "line":
        if line_val is None:
            return None
        s = line_val
    else:
        s = eval_js_arg(arg)
        if s is None:
            return None

    lines = [f"let chunks = word_wrap_line({js_str_to_rust(s)}, {width});"]
    for am in re.finditer(r"assert\.strictEqual\(chunks\.length,\s*(\d+)\)", body):
        lines.append(f"assert_eq!(chunks.len(), {am.group(1)});")
    for am in re.finditer(
        r"assert\.strictEqual\(chunks\[(\d+)\]!\.text,\s*(\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')\)",
        body,
    ):
        s2 = eval_js_arg(am.group(2))
        if s2 is None:
            return None
        lines.append(f"assert_eq!(chunks[{am.group(1)}].text, {js_str_to_rust(s2)});")
    if "reconstructed" in body:
        lines.append(
            "let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();"
        )
        lines.append(f"assert_eq!(reconstructed, {js_str_to_rust(s)});")
    if len(lines) < 2:
        return None
    return "\n".join("    " + l for l in lines)


SLUG = {
    "Backslash+Enter newline workaround": "backslash",
    "Kitty CSI-u handling": "kitty",
    "Unicode text editing behavior": "unicode",
    "Grapheme-aware text wrapping": "grapheme_wrap",
    "Word wrapping": "word_wrap",
    "Kill ring": "kill_ring",
    "Undo": "undo",
    "Character jump (Ctrl+])": "char_jump",
    "Sticky column": "sticky",
    "Paste marker atomic behavior": "paste_marker",
    "Autocomplete": "autocomplete",
}


def main():
    text = ORACLE.read_text()
    cases = extract_cases(text)
    by = categorize(text, cases)
    untranslated = []
    generated = 0
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    for cat, items in by.items():
        if cat in ("Prompt history navigation", "public state accessors", "Editor component"):
            # history already hand-ported
            if cat != "Editor component":
                continue
        rust_tests = []
        for name, is_async, body in items:
            tr = translate_simple(body)
            if tr is None:
                untranslated.append((cat, name))
                continue
            rust_tests.append(f"#[test]\nfn {rust_ident(name)}() {{\n{tr}\n}}\n")
            generated += 1
        if rust_tests:
            slug = SLUG.get(cat, rust_ident(cat))
            path = OUT_DIR / f"editor_oracle_{slug}.rs"
            content = f"//! Oracle editor tests — {cat}\n{HELPERS}\n" + "\n".join(
                rust_tests
            )
            path.write_text(content)
            print(f"wrote {path.name}: {len(rust_tests)} tests")

    print(f"auto-generated: {generated}")
    print(f"untranslated: {len(untranslated)}")
    for cat, name in untranslated:
        print(f"  [{cat}] {name}")
    Path("/tmp/untranslated.json").write_text(json.dumps(untranslated, indent=2))


if __name__ == "__main__":
    main()
