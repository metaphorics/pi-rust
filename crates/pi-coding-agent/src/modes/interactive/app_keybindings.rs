//! App-level keybinding catalog.
//!
//! Port of `core/keybindings.ts`: the app `KEYBINDINGS` table (TUI defaults +
//! `app.*` actions, keys verbatim) and `KeybindingsManager.create()` which
//! layers user overrides from `<agent-dir>/keybindings.json`.

use std::collections::HashMap;
use std::path::Path;

use pi_tui::keybindings::{
    KeybindingDefinition, KeybindingsConfig, KeybindingsManager, TUI_KEYBINDINGS,
};

/// Oracle `KEYBINDINGS` (core/keybindings.ts:64-207): TUI defaults plus the
/// app action defaults, ids and keys verbatim.
#[must_use]
pub fn app_keybinding_definitions() -> HashMap<&'static str, KeybindingDefinition> {
    let mut m = TUI_KEYBINDINGS.clone();
    let mut insert = |id: &'static str, keys: &[&'static str], desc: &'static str| {
        m.insert(
            id,
            KeybindingDefinition {
                default_keys: keys.to_vec(),
                description: Some(desc),
            },
        );
    };

    insert("app.interrupt", &["escape"], "Cancel or abort");
    insert("app.clear", &["ctrl+c"], "Clear editor");
    insert("app.exit", &["ctrl+d"], "Exit when editor is empty");
    insert("app.suspend", &["ctrl+z"], "Suspend to background");
    insert("app.thinking.cycle", &["shift+tab"], "Cycle thinking level");
    insert("app.model.cycleForward", &["ctrl+p"], "Cycle to next model");
    insert(
        "app.model.cycleBackward",
        &["shift+ctrl+p"],
        "Cycle to previous model",
    );
    insert("app.model.select", &["ctrl+l"], "Open model selector");
    insert("app.tools.expand", &["ctrl+o"], "Toggle tool output");
    insert("app.thinking.toggle", &["ctrl+t"], "Toggle thinking blocks");
    insert(
        "app.session.toggleNamedFilter",
        &["ctrl+n"],
        "Toggle named session filter",
    );
    insert("app.editor.external", &["ctrl+g"], "Open external editor");
    insert("app.message.copy", &["ctrl+x"], "Copy message to clipboard");
    insert(
        "app.message.followUp",
        &["alt+enter"],
        "Queue follow-up message",
    );
    insert(
        "app.message.dequeue",
        &["alt+up"],
        "Restore queued messages",
    );
    insert(
        "app.clipboard.pasteImage",
        &["ctrl+v"],
        "Paste image from clipboard (text fallback)",
    );
    insert("app.session.new", &[], "Start a new session");
    insert("app.session.tree", &[], "Open session tree");
    insert("app.session.fork", &[], "Fork current session");
    insert("app.session.resume", &[], "Resume a session");
    if cfg!(target_os = "macos") {
        insert(
            "app.tree.foldOrUp",
            &["alt+left", "ctrl+left"],
            "Fold tree branch or move up",
        );
        insert(
            "app.tree.unfoldOrDown",
            &["alt+right", "ctrl+right"],
            "Unfold tree branch or move down",
        );
    } else {
        insert(
            "app.tree.foldOrUp",
            &["ctrl+left", "alt+left"],
            "Fold tree branch or move up",
        );
        insert(
            "app.tree.unfoldOrDown",
            &["ctrl+right", "alt+right"],
            "Unfold tree branch or move down",
        );
    }
    insert("app.tree.editLabel", &["shift+l"], "Edit tree label");
    insert(
        "app.tree.toggleLabelTimestamp",
        &["shift+t"],
        "Toggle tree label timestamps",
    );
    insert(
        "app.session.togglePath",
        &["ctrl+p"],
        "Toggle session path display",
    );
    insert(
        "app.session.toggleSort",
        &["ctrl+s"],
        "Toggle session sort mode",
    );
    insert("app.session.rename", &["ctrl+r"], "Rename session");
    insert("app.session.delete", &["ctrl+d"], "Delete session");
    insert(
        "app.session.deleteNoninvasive",
        &["ctrl+backspace"],
        "Delete session when query is empty",
    );
    insert("app.models.save", &["ctrl+s"], "Save model selection");
    insert("app.models.enableAll", &["ctrl+a"], "Enable all models");
    insert("app.models.clearAll", &["ctrl+x"], "Clear all models");
    insert(
        "app.models.toggleProvider",
        &["ctrl+p"],
        "Toggle all models for provider",
    );
    insert(
        "app.models.reorderUp",
        &["alt+up"],
        "Move model up in order",
    );
    insert(
        "app.models.reorderDown",
        &["alt+down"],
        "Move model down in order",
    );
    insert(
        "app.tree.filter.default",
        &["ctrl+d"],
        "Tree filter: default view",
    );
    insert(
        "app.tree.filter.noTools",
        &["ctrl+t"],
        "Tree filter: hide tool results",
    );
    insert(
        "app.tree.filter.userOnly",
        &["ctrl+u"],
        "Tree filter: user messages only",
    );
    insert(
        "app.tree.filter.labeledOnly",
        &["ctrl+l"],
        "Tree filter: labeled entries only",
    );
    insert(
        "app.tree.filter.all",
        &["ctrl+a"],
        "Tree filter: show all entries",
    );
    insert(
        "app.tree.filter.cycleForward",
        &["ctrl+o"],
        "Tree filter: cycle forward",
    );
    insert(
        "app.tree.filter.cycleBackward",
        &["shift+ctrl+o"],
        "Tree filter: cycle backward",
    );

    m
}

/// Parse `<agent-dir>/keybindings.json` (id → key or key list). Unknown ids
/// are dropped by the manager; malformed files behave as absent (oracle
/// `loadRawConfig` swallows parse errors).
fn load_user_bindings(config_path: &Path) -> KeybindingsConfig {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return KeybindingsConfig::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return KeybindingsConfig::new();
    };
    let Some(map) = value.as_object() else {
        return KeybindingsConfig::new();
    };
    let mut config = KeybindingsConfig::new();
    for (id, keys) in map {
        let keys: Vec<String> = match keys {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => continue,
        };
        config.insert(id.clone(), keys);
    }
    config
}

/// Oracle `KeybindingsManager.create(agentDir)`: app definitions + user
/// overrides from `keybindings.json`.
#[must_use]
pub fn create_app_keybindings(agent_dir: &Path) -> KeybindingsManager {
    let user = load_user_bindings(&agent_dir.join("keybindings.json"));
    KeybindingsManager::new(app_keybinding_definitions(), user)
}
