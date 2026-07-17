//! Global keybinding registry — port of packages/tui/src/keybindings.ts.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use crate::keys::matches_key;

/// One keybinding definition (default keys + optional description).
#[derive(Debug, Clone)]
pub struct KeybindingDefinition {
    pub default_keys: Vec<&'static str>,
    pub description: Option<&'static str>,
}

/// User / resolved config: keybinding id → one or more key ids.
pub type KeybindingsConfig = HashMap<String, Vec<String>>;

/// Conflicting user claim of the same key by multiple keybinding ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

/// Default TUI keybindings — ids and keys verbatim from `TUI_KEYBINDINGS`.
pub static TUI_KEYBINDINGS: LazyLock<HashMap<&'static str, KeybindingDefinition>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        let mut insert = |id: &'static str, keys: &[&'static str], desc: &'static str| {
            m.insert(
                id,
                KeybindingDefinition {
                    default_keys: keys.to_vec(),
                    description: Some(desc),
                },
            );
        };

        insert("tui.editor.cursorUp", &["up"], "Move cursor up");
        insert("tui.editor.cursorDown", &["down"], "Move cursor down");
        insert(
            "tui.editor.cursorLeft",
            &["left", "ctrl+b"],
            "Move cursor left",
        );
        insert(
            "tui.editor.cursorRight",
            &["right", "ctrl+f"],
            "Move cursor right",
        );
        insert(
            "tui.editor.cursorWordLeft",
            &["alt+left", "ctrl+left", "alt+b"],
            "Move cursor word left",
        );
        insert(
            "tui.editor.cursorWordRight",
            &["alt+right", "ctrl+right", "alt+f"],
            "Move cursor word right",
        );
        insert(
            "tui.editor.cursorLineStart",
            &["home", "ctrl+a"],
            "Move to line start",
        );
        insert(
            "tui.editor.cursorLineEnd",
            &["end", "ctrl+e"],
            "Move to line end",
        );
        insert(
            "tui.editor.jumpForward",
            &["ctrl+]"],
            "Jump forward to character",
        );
        insert(
            "tui.editor.jumpBackward",
            &["ctrl+alt+]"],
            "Jump backward to character",
        );
        insert("tui.editor.pageUp", &["pageUp"], "Page up");
        insert("tui.editor.pageDown", &["pageDown"], "Page down");
        insert(
            "tui.editor.deleteCharBackward",
            &["backspace"],
            "Delete character backward",
        );
        insert(
            "tui.editor.deleteCharForward",
            &["delete", "ctrl+d"],
            "Delete character forward",
        );
        insert(
            "tui.editor.deleteWordBackward",
            &["ctrl+w", "alt+backspace"],
            "Delete word backward",
        );
        insert(
            "tui.editor.deleteWordForward",
            &["alt+d", "alt+delete"],
            "Delete word forward",
        );
        insert(
            "tui.editor.deleteToLineStart",
            &["ctrl+u"],
            "Delete to line start",
        );
        insert(
            "tui.editor.deleteToLineEnd",
            &["ctrl+k"],
            "Delete to line end",
        );
        insert("tui.editor.yank", &["ctrl+y"], "Yank");
        insert("tui.editor.yankPop", &["alt+y"], "Yank pop");
        insert("tui.editor.undo", &["ctrl+-"], "Undo");
        insert(
            "tui.input.newLine",
            &["shift+enter", "ctrl+j"],
            "Insert newline",
        );
        insert("tui.input.submit", &["enter"], "Submit input");
        insert("tui.input.tab", &["tab"], "Tab / autocomplete");
        insert("tui.input.copy", &["ctrl+c"], "Copy selection");
        insert("tui.select.up", &["up"], "Move selection up");
        insert("tui.select.down", &["down"], "Move selection down");
        insert("tui.select.pageUp", &["pageUp"], "Selection page up");
        insert("tui.select.pageDown", &["pageDown"], "Selection page down");
        insert("tui.select.confirm", &["enter"], "Confirm selection");
        insert(
            "tui.select.cancel",
            &["escape", "ctrl+c"],
            "Cancel selection",
        );

        m
    });

fn normalize_keys(keys: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for key in keys {
        if seen.insert(key.clone()) {
            result.push(key.clone());
        }
    }
    result
}

fn normalize_static_keys(keys: &[&'static str]) -> Vec<String> {
    let owned: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
    normalize_keys(&owned)
}

/// Resolves keybinding ids to key sequences and matches raw terminal input.
#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    definitions: HashMap<String, KeybindingDefinition>,
    user_bindings: KeybindingsConfig,
    keys_by_id: HashMap<String, Vec<String>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    /// Build from default TUI definitions (no user overrides).
    pub fn with_defaults() -> Self {
        Self::new(TUI_KEYBINDINGS.clone(), KeybindingsConfig::new())
    }

    /// `definitions` maps keybinding id → definition (static defaults or custom).
    pub fn new(
        definitions: HashMap<&'static str, KeybindingDefinition>,
        user_bindings: KeybindingsConfig,
    ) -> Self {
        let definitions: HashMap<String, KeybindingDefinition> = definitions
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        Self::from_owned(definitions, user_bindings)
    }

    /// Build from already-owned definition map.
    pub fn from_owned(
        definitions: HashMap<String, KeybindingDefinition>,
        user_bindings: KeybindingsConfig,
    ) -> Self {
        let mut mgr = Self {
            definitions,
            user_bindings,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
        };
        mgr.rebuild();
        mgr
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        let mut user_claims: HashMap<String, HashSet<String>> = HashMap::new();
        for (keybinding, keys) in &self.user_bindings {
            if !self.definitions.contains_key(keybinding) {
                continue;
            }
            for key in normalize_keys(keys) {
                user_claims
                    .entry(key)
                    .or_default()
                    .insert(keybinding.clone());
            }
        }

        for (key, keybindings) in &user_claims {
            if keybindings.len() > 1 {
                let mut ids: Vec<String> = keybindings.iter().cloned().collect();
                ids.sort();
                self.conflicts.push(KeybindingConflict {
                    key: key.clone(),
                    keybindings: ids,
                });
            }
        }

        for (id, definition) in &self.definitions {
            let keys = match self.user_bindings.get(id) {
                None => normalize_static_keys(&definition.default_keys),
                Some(user_keys) => normalize_keys(user_keys),
            };
            self.keys_by_id.insert(id.clone(), keys);
        }
    }

    /// True if raw terminal `data` matches any key bound to `id`.
    pub fn matches(&self, data: &str, id: &str) -> bool {
        let Some(keys) = self.keys_by_id.get(id) else {
            return false;
        };
        for key in keys {
            if matches_key(data, key) {
                return true;
            }
        }
        false
    }

    pub fn get_keys(&self, id: &str) -> Vec<String> {
        self.keys_by_id.get(id).cloned().unwrap_or_default()
    }

    pub fn get_definition(&self, id: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(id)
    }

    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        let mut resolved = KeybindingsConfig::new();
        for id in self.definitions.keys() {
            let keys = self.keys_by_id.get(id).cloned().unwrap_or_default();
            resolved.insert(id.clone(), keys);
        }
        resolved
    }
}

static GLOBAL_KEYBINDINGS: LazyLock<Mutex<Arc<KeybindingsManager>>> =
    LazyLock::new(|| Mutex::new(Arc::new(KeybindingsManager::with_defaults())));

/// Replace the process-global keybindings manager.
pub fn set_keybindings(keybindings: KeybindingsManager) {
    let keybindings = Arc::new(keybindings);
    match GLOBAL_KEYBINDINGS.lock() {
        Ok(mut guard) => {
            *guard = keybindings;
        }
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            *guard = keybindings;
        }
    }
}

/// Process-global keybindings manager snapshot (creates defaults on first
/// use).
///
/// Returned as an `Arc` snapshot — never a held guard — so nested
/// `handle_input` chains (SettingsList search → Input, Editor autocomplete →
/// SelectList) cannot deadlock on the registry lock.
pub fn get_keybindings() -> Arc<KeybindingsManager> {
    match GLOBAL_KEYBINDINGS.lock() {
        Ok(guard) => Arc::clone(&guard),
        Err(poisoned) => Arc::clone(&poisoned.into_inner()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contain_editor_submit() {
        let mgr = KeybindingsManager::with_defaults();
        assert!(mgr.get_keys("tui.input.submit").contains(&"enter".into()));
        assert!(
            mgr.get_keys("tui.editor.cursorLeft")
                .iter()
                .any(|k| k == "left" || k == "ctrl+b")
        );
    }

    #[test]
    fn matches_enter() {
        let mgr = KeybindingsManager::with_defaults();
        assert!(mgr.matches("\r", "tui.input.submit") || mgr.matches("\n", "tui.input.submit"));
    }

    #[test]
    fn global_get_set() {
        let mgr = KeybindingsManager::with_defaults();
        set_keybindings(mgr);
        let g = get_keybindings();
        assert!(!g.get_keys("tui.editor.undo").is_empty());
    }
}
