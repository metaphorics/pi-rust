//! Oracle editor tests — Backslash+Enter newline workaround
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


fn force_files(items: Vec<(&str, &str)>) -> Box<dyn AutocompleteProvider> {
    struct P { items: Vec<AutocompleteItem> }
    impl AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            _lines: &[String],
            _line: usize,
            _col: usize,
            options: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            if !options.force {
                return None;
            }
            Some(AutocompleteSuggestions {
                items: self.items.clone(),
                prefix: String::new(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
        fn should_trigger_file_completion(&self, _: &[String], _: usize, _: usize) -> bool {
            true
        }
    }
    Box::new(P {
        items: items
            .into_iter()
            .map(|(v, l)| AutocompleteItem {
                value: v.into(),
                label: l.into(),
                description: None,
            })
            .collect(),
    })
}

#[test]
fn auto_applies_single_force_file_suggestion_without_showing_menu() {
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(force_files(vec![("Work", "Work")]));
    e.handle_input("\t");
    assert_eq!(e.get_text(), "Work");
    assert!(!e.is_showing_autocomplete());
}

#[test]
fn shows_menu_when_force_file_has_multiple_suggestions() {
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(force_files(vec![
        ("a.txt", "a.txt"),
        ("b.txt", "b.txt"),
    ]));
    e.handle_input("\t");
    assert!(e.is_showing_autocomplete());
    e.handle_input("\x1b"); // cancel
    assert!(!e.is_showing_autocomplete());
}

#[test]
fn debounces_at_autocomplete_while_typing() {
    struct P { calls: Arc<Mutex<usize>> }
    impl AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            *self.calls.lock().unwrap() += 1;
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "@main.rs".into(),
                    label: "@main.rs".into(),
                    description: None,
                }],
                prefix: before.to_owned(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    let calls = Arc::new(Mutex::new(0usize));
    e.set_autocomplete_provider(Box::new(P { calls: calls.clone() }));
    e.handle_input("@");
    e.handle_input("m");
    assert_eq!(*calls.lock().unwrap(), 0);
    assert!(!e.is_showing_autocomplete());
    e.flush_autocomplete();
    assert!(*calls.lock().unwrap() >= 1);
    assert!(e.is_showing_autocomplete());
}

#[test]
fn debounces_hash_autocomplete_while_typing() {
    struct P;
    impl AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "#tag".into(),
                    label: "#tag".into(),
                    description: None,
                }],
                prefix: before.to_owned(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P));
    e.handle_input("#");
    e.handle_input("t");
    assert!(!e.is_showing_autocomplete());
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
}

#[test]
fn debounces_custom_trigger_characters_autocomplete_while_typing() {
    struct P;
    impl AutocompleteProvider for P {
        fn trigger_characters(&self) -> &[char] {
            &['$']
        }
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "$var".into(),
                    label: "$var".into(),
                    description: None,
                }],
                prefix: before.to_owned(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P));
    e.handle_input("$");
    e.handle_input("v");
    assert!(!e.is_showing_autocomplete());
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
}

#[test]
fn resets_custom_trigger_characters_when_provider_changes() {
    struct P1;
    impl AutocompleteProvider for P1 {
        fn trigger_characters(&self) -> &[char] {
            &['$']
        }
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "$x".into(),
                    label: "$x".into(),
                    description: None,
                }],
                prefix: before.to_owned(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    struct P2;
    impl AutocompleteProvider for P2 {
        fn get_suggestions(
            &self,
            _: &[String],
            _: usize,
            _: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            None
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P1));
    e.set_autocomplete_provider(Box::new(P2));
    e.handle_input("$");
    e.flush_autocomplete();
    assert!(!e.is_showing_autocomplete());
}

#[test]
fn aborts_active_at_autocomplete_when_typing_continues() {
    struct P {
        aborts: Arc<Mutex<usize>>,
        delay: bool,
    }
    impl AutocompleteProvider for P {
        fn begin_suggestions(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            options: SuggestionOptions,
        ) -> SuggestionStart {
            let aborts = self.aborts.clone();
            options.cancel.on_cancel(move || {
                *aborts.lock().unwrap() += 1;
            });
            if self.delay {
                let (tx, rx) = mpsc::channel();
                let lines = lines.to_vec();
                let cancel = options.cancel.clone();
                thread::spawn(move || {
                    // Simulate slow provider; abort should fire before send if cancelled.
                    thread::sleep(Duration::from_millis(50));
                    if cancel.is_cancelled() {
                        let _ = tx.send(None);
                        return;
                    }
                    let before = &lines[line][..utf16_to_byte(&lines[line], col.min(utf16_len(&lines[line])))];
                    let _ = tx.send(Some(AutocompleteSuggestions {
                        items: vec![AutocompleteItem {
                            value: "@main.rs".into(),
                            label: "@main.rs".into(),
                            description: None,
                        }],
                        prefix: before.to_owned(),
                    }));
                });
                SuggestionStart::Pending(rx)
            } else {
                SuggestionStart::Ready(self.get_suggestions(lines, line, col, options))
            }
        }
        fn get_suggestions(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            options: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            if options.aborted() {
                return None;
            }
            let before = &lines[line][..utf16_to_byte(&lines[line], col.min(utf16_len(&lines[line])))];
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "@main.rs".into(),
                    label: "@main.rs".into(),
                    description: None,
                }],
                prefix: before.to_owned(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    let aborts = Arc::new(Mutex::new(0usize));
    e.set_autocomplete_debounce_ms(0);
    e.set_autocomplete_provider(Box::new(P {
        aborts: aborts.clone(),
        delay: true,
    }));
    e.handle_input("@");
    // First request starts pending
    e.flush_autocomplete();
    // Typing continues -> cancel previous
    e.handle_input("m");
    e.flush_autocomplete();
    assert_eq!(*aborts.lock().unwrap(), 1);
}

#[test]
fn hides_autocomplete_when_backspacing_slash_command_to_empty() {
    struct P;
    impl AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            if !before.starts_with('/') {
                return None;
            }
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "help".into(),
                    label: "help".into(),
                    description: None,
                }],
                prefix: before.to_owned(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P));
    e.handle_input("/");
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
    e.handle_input("\x7f");
    e.flush_autocomplete();
    assert!(!e.is_showing_autocomplete());
}

#[test]
fn applies_exact_typed_slash_argument_value_on_enter() {
    struct P;
    impl AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            let text = &lines[0];
            let before = &text[..utf16_to_byte(text, col)];
            let re = regex::Regex::new(r"^/argtest\s+(\S+)$").unwrap();
            let caps = re.captures(before)?;
            let argument_text = caps.get(1)?.as_str();
            let all = ["one", "two", "three"];
            let filtered: Vec<_> = all
                .iter()
                .filter(|a| a.starts_with(argument_text))
                .map(|a| AutocompleteItem {
                    value: (*a).into(),
                    label: (*a).into(),
                    description: None,
                })
                .collect();
            if filtered.is_empty() {
                return None;
            }
            Some(AutocompleteSuggestions {
                items: filtered,
                prefix: argument_text.into(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P));
    for ch in "/argtest two".chars() {
        e.handle_input(&ch.to_string());
    }
    assert_eq!(e.get_text(), "/argtest two");
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
    e.handle_input("\r");
    assert_eq!(e.get_text(), "/argtest two");
}

#[test]
fn selects_first_prefix_match_on_enter_when_typed_arg_not_exact() {
    struct P;
    impl AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            let text = &lines[0];
            let before = &text[..utf16_to_byte(text, col)];
            let re = regex::Regex::new(r"^/argtest\s+(\S+)$").unwrap();
            let caps = re.captures(before)?;
            let argument_text = caps.get(1)?.as_str();
            let all = ["two", "three", "twelve"];
            let filtered: Vec<_> = all
                .iter()
                .filter(|a| a.starts_with(argument_text))
                .map(|a| AutocompleteItem {
                    value: (*a).into(),
                    label: (*a).into(),
                    description: None,
                })
                .collect();
            if filtered.is_empty() {
                return None;
            }
            Some(AutocompleteSuggestions {
                items: filtered,
                prefix: argument_text.into(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P));
    for ch in "/argtest t".chars() {
        e.handle_input(&ch.to_string());
    }
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
    e.handle_input("\r");
    assert_eq!(e.get_text(), "/argtest two");
}

#[test]
fn awaits_async_slash_command_argument_completions() {
    fn load_skills_args(prefix: &str) -> Option<Vec<AutocompleteItem>> {
        if prefix.starts_with('s') {
            Some(vec![AutocompleteItem {
                value: "skill-a".into(),
                label: "skill-a".into(),
                description: None,
            }])
        } else {
            None
        }
    }
    let t = tui();
    let mut e = editor(&t);
    let provider = CombinedAutocompleteProvider::new(
        vec![SlashCommand::new("load-skills")
            .with_description("Load skills")
            .with_argument_completions(load_skills_args)
            .into()],
        ".",
    );
    e.set_autocomplete_provider(Box::new(provider));
    e.set_text("/load-skills ");
    e.handle_input("s");
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
    e.handle_input("\t");
    assert_eq!(e.get_text(), "/load-skills skill-a");
    assert!(!e.is_showing_autocomplete());
}

#[test]
fn does_not_show_argument_completions_when_command_has_no_argument_completer() {
    let t = tui();
    let mut e = editor(&t);
    let provider = CombinedAutocompleteProvider::new(
        vec![
            SlashCommand::new("help").with_description("Show help").into(),
            SlashCommand::new("model")
                .with_description("Switch model")
                .with_argument_completions(|_| {
                    Some(vec![AutocompleteItem {
                        value: "claude-opus".into(),
                        label: "claude-opus".into(),
                        description: None,
                    }])
                })
                .into(),
        ],
        ".",
    );
    e.set_autocomplete_provider(Box::new(provider));
    e.handle_input("/");
    e.handle_input("h");
    e.handle_input("e");
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
    e.handle_input("\t");
    assert_eq!(e.get_text(), "/help ");
    assert!(!e.is_showing_autocomplete());
}

#[test]
fn undoes_autocomplete() {
    struct P;
    impl AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            options: SuggestionOptions,
        ) -> Option<AutocompleteSuggestions> {
            if !options.force {
                return None;
            }
            let _ = (lines, col);
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "completed".into(),
                    label: "completed".into(),
                    description: None,
                }],
                prefix: String::new(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            line: usize,
            col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> AppliedCompletion {
            apply_completion(lines, line, col, item, prefix)
        }
        fn should_trigger_file_completion(&self, _: &[String], _: usize, _: usize) -> bool {
            true
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P));
    e.handle_input("\t");
    assert_eq!(e.get_text(), "completed");
    e.handle_input("\x1b[45;5u");
    assert_eq!(e.get_text(), "");
}

#[test]
fn keeps_suggestions_open_when_typing_in_force_mode() {
    struct P;
    impl pi_tui::autocomplete::AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            options: pi_tui::autocomplete::SuggestionOptions,
        ) -> Option<pi_tui::autocomplete::AutocompleteSuggestions> {
            if !options.force {
                return None;
            }
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            let all = ["readme.md", "package.json", "src/main.rs"];
            let items: Vec<_> = all
                .iter()
                .filter(|f| f.starts_with(before) || before.is_empty())
                .map(|f| pi_tui::autocomplete::AutocompleteItem {
                    value: (*f).into(),
                    label: (*f).into(),
                    description: None,
                })
                .collect();
            Some(pi_tui::autocomplete::AutocompleteSuggestions {
                items,
                prefix: before.to_owned(),
            })
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
        fn should_trigger_file_completion(&self, _: &[String], _: usize, _: usize) -> bool {
            true
        }
    }
    let t = tui();
    let mut e = editor(&t);
    e.set_autocomplete_provider(Box::new(P));
    e.handle_input("\t");
    assert!(e.is_showing_autocomplete());
    e.handle_input("r");
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
}

#[test]
fn re_queries_autocomplete_when_cursor_moves_back_into_command_name() {
    struct P;
    impl pi_tui::autocomplete::AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: pi_tui::autocomplete::SuggestionOptions,
        ) -> Option<pi_tui::autocomplete::AutocompleteSuggestions> {
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            if before == "/cmd " {
                return Some(pi_tui::autocomplete::AutocompleteSuggestions {
                    items: vec![
                        pi_tui::autocomplete::AutocompleteItem {
                            value: "message".into(),
                            label: "message".into(),
                            description: None,
                        },
                        pi_tui::autocomplete::AutocompleteItem {
                            value: "mode".into(),
                            label: "mode".into(),
                            description: None,
                        },
                    ],
                    prefix: String::new(),
                });
            }
            if before.starts_with("/cm") {
                return Some(pi_tui::autocomplete::AutocompleteSuggestions {
                    items: vec![pi_tui::autocomplete::AutocompleteItem {
                        value: "cmd".into(),
                        label: "cmd".into(),
                        description: None,
                    }],
                    prefix: before.to_owned(),
                });
            }
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
    e.set_autocomplete_provider(Box::new(P));
    for ch in "/cmd ".chars() {
        e.handle_input(&ch.to_string());
    }
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
    let before_move = render_plain(&mut e, 80);
    let before_join = before_move.join("\n");
    assert!(before_join.contains("message"));
    e.handle_input("\x1b[D"); // left out of arg region
    e.flush_autocomplete();
    let after_move = render_plain(&mut e, 80).join("\n");
    assert!(!after_move.contains("message"), "stale argument menu must not survive");
}

#[test]
fn works_for_built_in_style_command_argument_completion_path() {
    struct P;
    impl pi_tui::autocomplete::AutocompleteProvider for P {
        fn get_suggestions(
            &self,
            lines: &[String],
            _: usize,
            col: usize,
            _: pi_tui::autocomplete::SuggestionOptions,
        ) -> Option<pi_tui::autocomplete::AutocompleteSuggestions> {
            let before = &lines[0][..utf16_to_byte(&lines[0], col)];
            let re = regex::Regex::new(r"^/model\s+(\S*)$").unwrap();
            let caps = re.captures(before)?;
            let prefix = caps.get(1)?.as_str();
            let all = ["gpt-4o", "gpt-4o-mini", "o1-preview"];
            let items: Vec<_> = all
                .iter()
                .filter(|m| m.starts_with(prefix))
                .map(|m| pi_tui::autocomplete::AutocompleteItem {
                    value: (*m).into(),
                    label: (*m).into(),
                    description: None,
                })
                .collect();
            if items.is_empty() {
                return None;
            }
            Some(pi_tui::autocomplete::AutocompleteSuggestions {
                items,
                prefix: prefix.into(),
            })
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
    e.set_autocomplete_provider(Box::new(P));
    for ch in "/model gpt-4o-m".chars() {
        e.handle_input(&ch.to_string());
    }
    e.flush_autocomplete();
    assert!(e.is_showing_autocomplete());
    e.handle_input("\r");
    assert_eq!(e.get_text(), "/model gpt-4o-mini");
}

#[test]
fn ignores_invalid_slash_command_argument_completion_results() {
    // CombinedAutocompleteProvider with completer returning None for invalid.
    fn bad(_prefix: &str) -> Option<Vec<pi_tui::autocomplete::AutocompleteItem>> {
        None
    }
    let t = tui();
    let mut e = editor(&t);
    let provider = pi_tui::autocomplete::CombinedAutocompleteProvider::new(
        vec![pi_tui::autocomplete::SlashCommand::new("load-skills")
            .with_argument_completions(bad)
            .into()],
        ".",
    );
    e.set_autocomplete_provider(Box::new(provider));
    e.set_text("/load-skills ");
    e.handle_input("s");
    e.flush_autocomplete();
    assert!(!e.is_showing_autocomplete());
    assert_eq!(e.get_text(), "/load-skills s");
}
