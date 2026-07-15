//! Autocomplete provider trait and combined slash/path skeleton.
//!
//! Port of `packages/tui/src/autocomplete.ts` — trait surface + applyCompletion
//! logic sufficient for editor tests. Full `fd`-backed fuzzy path walk can land
//! later; `get_suggestions` is a sync stub with optional slash-command matching.

use std::path::{Path, PathBuf};

use crate::fuzzy::fuzzy_filter;

/// One selectable autocomplete entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Slash command descriptor for CombinedAutocompleteProvider.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// Optional argument completer. Returns `None` when no completions apply.
    pub get_argument_completions: Option<fn(&str) -> Option<Vec<AutocompleteItem>>>,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            argument_hint: None,
            get_argument_completions: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_argument_hint(mut self, hint: impl Into<String>) -> Self {
        self.argument_hint = Some(hint.into());
        self
    }
}

/// Suggestion list plus the matched prefix text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    /// What we're matching against (e.g. `"/"` or `"src/"`).
    pub prefix: String,
}

/// Options for [`AutocompleteProvider::get_suggestions`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SuggestionOptions {
    /// Explicit Tab force (path completion even without path-like prefix).
    pub force: bool,
}

/// Result of applying a selected completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedCompletion {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// Autocomplete provider contract (sync stub of the TS async API).
///
/// TS uses `Promise` + `AbortSignal`; here callers run on the TUI thread and
/// pass [`SuggestionOptions`]. Async wrappers can spawn and call these methods.
pub trait AutocompleteProvider {
    /// Characters that naturally trigger this provider at token boundaries.
    fn trigger_characters(&self) -> &[char] {
        &[]
    }

    /// Suggestions for current text/cursor. `None` = no suggestions.
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> Option<AutocompleteSuggestions>;

    /// Apply the selected item; returns new lines + cursor.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> AppliedCompletion;

    /// Whether file completion should trigger for explicit Tab.
    fn should_trigger_file_completion(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
    ) -> bool {
        false
    }
}

/// Command entry accepted by CombinedAutocompleteProvider.
#[derive(Debug, Clone)]
pub enum CommandEntry {
    Slash(SlashCommand),
    Item(AutocompleteItem),
}

impl From<SlashCommand> for CommandEntry {
    fn from(c: SlashCommand) -> Self {
        CommandEntry::Slash(c)
    }
}

impl From<AutocompleteItem> for CommandEntry {
    fn from(i: AutocompleteItem) -> Self {
        CommandEntry::Item(i)
    }
}

/// Combined slash-command + path provider skeleton for editor tests.
///
/// Path walking via `fd` is not wired; force/path suggestions return empty
/// unless a simple in-memory file list is injected via [`Self::with_files`].
pub struct CombinedAutocompleteProvider {
    commands: Vec<CommandEntry>,
    base_path: PathBuf,
    /// Optional in-memory relative paths for tests (avoids spawning `fd`).
    files: Vec<String>,
    trigger_chars: Vec<char>,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<CommandEntry>, base_path: impl Into<PathBuf>) -> Self {
        Self {
            commands,
            base_path: base_path.into(),
            files: Vec::new(),
            trigger_chars: vec!['/', '@'],
        }
    }

    /// Inject relative file paths for unit tests (no filesystem walk).
    pub fn with_files(mut self, files: Vec<String>) -> Self {
        self.files = files;
        self
    }

    fn command_items(&self) -> Vec<(String, String, Option<String>)> {
        self.commands
            .iter()
            .map(|cmd| match cmd {
                CommandEntry::Slash(c) => {
                    let desc = match (&c.argument_hint, &c.description) {
                        (Some(h), Some(d)) => Some(format!("{h} — {d}")),
                        (Some(h), None) => Some(h.clone()),
                        (None, Some(d)) => Some(d.clone()),
                        (None, None) => None,
                    };
                    (c.name.clone(), c.name.clone(), desc)
                }
                CommandEntry::Item(i) => (i.value.clone(), i.label.clone(), i.description.clone()),
            })
            .collect()
    }

    fn slash_command_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let items = self.command_items();
        let names: Vec<&str> = items.iter().map(|(n, _, _)| n.as_str()).collect();
        let filtered = fuzzy_filter(&names, prefix, |s| *s);
        filtered
            .into_iter()
            .filter_map(|name| {
                items
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(value, label, description)| AutocompleteItem {
                        value: value.clone(),
                        label: label.clone(),
                        description: description.clone(),
                    })
            })
            .collect()
    }

    fn extract_at_prefix(text: &str) -> Option<String> {
        if let Some(q) = extract_quoted_prefix(text)
            && q.starts_with("@\"") {
                return Some(q);
            }
        let last = find_last_delimiter(text);
        let token_start = if last == usize::MAX { 0 } else { last + 1 };
        if text.as_bytes().get(token_start) == Some(&b'@') {
            Some(text[token_start..].to_string())
        } else {
            None
        }
    }

    fn extract_path_prefix(text: &str, force_extract: bool) -> Option<String> {
        if let Some(q) = extract_quoted_prefix(text) {
            return Some(q);
        }
        let last = find_last_delimiter(text);
        let path_prefix = if last == usize::MAX {
            text.to_string()
        } else {
            text[last + 1..].to_string()
        };
        if force_extract {
            return Some(path_prefix);
        }
        if path_prefix.contains('/')
            || path_prefix.starts_with('.')
            || path_prefix.starts_with("~/")
        {
            return Some(path_prefix);
        }
        if path_prefix.is_empty() && text.ends_with(' ') {
            return Some(path_prefix);
        }
        None
    }

    fn file_suggestions(&self, raw_prefix: &str) -> Vec<AutocompleteItem> {
        if self.files.is_empty() {
            return Vec::new();
        }
        let query = raw_prefix.trim_start_matches('@');
        let query = query.trim_matches('"');
        let filtered: Vec<&str> = if query.is_empty() {
            self.files.iter().map(String::as_str).take(50).collect()
        } else {
            let names: Vec<&str> = self.files.iter().map(String::as_str).collect();
            fuzzy_filter(&names, query, |s| *s)
                .into_iter()
                .copied()
                .collect()
        };
        filtered
            .into_iter()
            .map(|path| {
                let is_dir = path.ends_with('/');
                AutocompleteItem {
                    value: path.to_string(),
                    label: path.to_string(),
                    description: if is_dir {
                        Some("directory".into())
                    } else {
                        None
                    },
                }
            })
            .collect()
    }

    fn argument_completions(
        &self,
        command_name: &str,
        argument_prefix: &str,
    ) -> Option<Vec<AutocompleteItem>> {
        for cmd in &self.commands {
            if let CommandEntry::Slash(c) = cmd
                && c.name == command_name {
                    return c.get_argument_completions.and_then(|f| f(argument_prefix));
                }
        }
        None
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn trigger_characters(&self) -> &[char] {
        &self.trigger_chars
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> Option<AutocompleteSuggestions> {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let col = cursor_col.min(current.len());
        let text_before = &current[..col];

        if let Some(at_prefix) = Self::extract_at_prefix(text_before) {
            let raw = at_prefix.trim_start_matches('@');
            let suggestions = self.file_suggestions(raw);
            if suggestions.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items: suggestions,
                prefix: at_prefix,
            });
        }

        if !options.force && text_before.starts_with('/') {
            let space_index = text_before.find(' ');
            if space_index.is_none() {
                let prefix = &text_before[1..];
                let items = self.slash_command_suggestions(prefix);
                if items.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions {
                    items,
                    prefix: text_before.to_string(),
                });
            }
            // `/cmd arg…` — argument completions
            let space = space_index.unwrap();
            let cmd_name = &text_before[1..space];
            let arg_prefix = &text_before[space + 1..];
            if let Some(items) = self.argument_completions(cmd_name, arg_prefix) {
                if items.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions {
                    items,
                    prefix: arg_prefix.to_string(),
                });
            }
        }

        if let Some(path_match) = Self::extract_path_prefix(text_before, options.force) {
            let suggestions = self.file_suggestions(&path_match);
            if suggestions.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items: suggestions,
                prefix: path_match,
            });
        }

        let _ = &self.base_path; // reserved for future fs walk
        None
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> AppliedCompletion {
        let current = lines.get(cursor_line).cloned().unwrap_or_default();
        let col = cursor_col.min(current.len());
        let prefix_len = prefix.len().min(col);
        let before_prefix = &current[..col - prefix_len];
        let after_cursor = &current[col..];
        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let has_leading_quote_after = after_cursor.starts_with('"');
        let has_trailing_quote_in_item = item.value.ends_with('"');
        let adjusted_after =
            if is_quoted_prefix && has_trailing_quote_in_item && has_leading_quote_after {
                &after_cursor[1..]
            } else {
                after_cursor
            };

        let is_slash_command = prefix.starts_with('/')
            && before_prefix.trim().is_empty()
            && !prefix[1..].contains('/');

        let (new_line, new_col) = if is_slash_command {
            let new_line = format!("{before_prefix}/{} {adjusted_after}", item.value);
            let new_col = before_prefix.len() + item.value.len() + 2;
            (new_line, new_col)
        } else if prefix.starts_with('@') {
            let is_directory = item.label.ends_with('/');
            let suffix = if is_directory { "" } else { " " };
            let new_line = format!("{before_prefix}{}{suffix}{adjusted_after}", item.value);
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.len() - 1
            } else {
                item.value.len()
            };
            (new_line, before_prefix.len() + cursor_offset + suffix.len())
        } else {
            let is_directory = item.label.ends_with('/');
            let new_line = format!("{before_prefix}{}{adjusted_after}", item.value);
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.len() - 1
            } else {
                item.value.len()
            };
            (new_line, before_prefix.len() + cursor_offset)
        };

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

    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let col = cursor_col.min(current.len());
        let text_before = &current[..col];
        Self::extract_path_prefix(text_before, true).is_some()
            || Self::extract_at_prefix(text_before).is_some()
    }
}

const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];

fn find_last_delimiter(text: &str) -> usize {
    text.char_indices()
        .filter(|(_, c)| PATH_DELIMITERS.contains(c))
        .map(|(i, _)| i)
        .next_back()
        .unwrap_or(usize::MAX)
}

fn extract_quoted_prefix(text: &str) -> Option<String> {
    // Find unclosed `"` or `@"` starting a token.
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            // token start?
            if i == 0 || PATH_DELIMITERS.contains(&(bytes[i - 1] as char)) {
                let rest = &text[i + 1..];
                if !rest.contains('"') {
                    return Some(text[i..].to_string());
                }
            }
        }
        if bytes[i] == b'@' && i + 1 < bytes.len() && bytes[i + 1] == b'"'
            && (i == 0 || PATH_DELIMITERS.contains(&(bytes[i - 1] as char))) {
                let rest = &text[i + 2..];
                if !rest.contains('"') {
                    return Some(text[i..].to_string());
                }
            }
        i += 1;
    }
    None
}

/// Expand `~/…` against home directory for future path resolution.
#[allow(dead_code)]
fn expand_home_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = env_home() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Some(home) = env_home() {
            return home;
        }
    PathBuf::from(path)
}

fn env_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

use std::env;

#[allow(dead_code)]
fn join_base(base: &Path, rel: &str) -> PathBuf {
    base.join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_suggestions() {
        let provider = CombinedAutocompleteProvider::new(
            vec![
                SlashCommand::new("help")
                    .with_description("show help")
                    .into(),
                SlashCommand::new("hello").into(),
            ],
            ".",
        );
        let lines = vec!["/he".into()];
        let s = provider
            .get_suggestions(&lines, 0, 3, SuggestionOptions::default())
            .expect("suggestions");
        assert!(!s.items.is_empty());
        assert!(
            s.items
                .iter()
                .any(|i| i.value == "help" || i.value == "hello")
        );
    }

    #[test]
    fn apply_slash_completion() {
        let provider =
            CombinedAutocompleteProvider::new(vec![SlashCommand::new("help").into()], ".");
        let lines = vec!["/he".into()];
        let item = AutocompleteItem {
            value: "help".into(),
            label: "help".into(),
            description: None,
        };
        let applied = provider.apply_completion(&lines, 0, 3, &item, "/he");
        assert_eq!(applied.lines[0], "/help ");
        assert_eq!(applied.cursor_col, 6);
    }

    #[test]
    fn file_suggestions_from_injected_list() {
        let provider = CombinedAutocompleteProvider::new(vec![], ".")
            .with_files(vec!["src/main.rs".into(), "src/lib.rs".into()]);
        let lines = vec!["src/".into()];
        let s = provider
            .get_suggestions(&lines, 0, 4, SuggestionOptions { force: false })
            .expect("file suggestions");
        assert_eq!(s.items.len(), 2);
    }
}
