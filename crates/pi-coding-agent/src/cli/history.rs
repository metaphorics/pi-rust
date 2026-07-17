//! Persistent editor prompt history — `~/.pi/agent/history.jsonl`.
//!
//! pi 0.80.7 keeps editor history session-scoped and in-memory
//! (packages/tui editor.ts:298-391); pi-rust persists submissions globally
//! so Up-arrow history survives restarts. Format: JSONL, one
//! `{"text": "..."}` object per line, oldest first; the file is
//! append-only at runtime and truncated to [`HISTORY_LIMIT`] entries on
//! load-rewrite.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::get_agent_dir;

/// Editor history cap (matches the editor's in-memory cap).
pub const HISTORY_LIMIT: usize = 100;

#[derive(Serialize, Deserialize)]
struct HistoryLine {
    text: String,
}

/// `~/.pi/agent/history.jsonl` (honors the agent-dir env overrides).
pub fn get_history_path() -> PathBuf {
    get_agent_dir().join("history.jsonl")
}

/// Load persisted history entries, oldest first, capped to the most recent
/// [`HISTORY_LIMIT`] with consecutive duplicates collapsed. Unreadable or
/// malformed lines are skipped (a corrupt history never blocks startup).
pub fn load_history(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries: Vec<String> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<HistoryLine>(line) else {
            continue;
        };
        let text = parsed.text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if entries.last() == Some(&text) {
            continue;
        }
        entries.push(text);
    }
    if entries.len() > HISTORY_LIMIT {
        entries.drain(..entries.len() - HISTORY_LIMIT);
    }
    entries
}

/// Append one submitted prompt. Errors are swallowed: history persistence
/// must never break a prompt submission.
pub fn append_history(path: &Path, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(&HistoryLine {
        text: trimmed.to_string(),
    }) else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_caps_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");

        append_history(&path, "first");
        append_history(&path, "  second  ");
        append_history(&path, ""); // ignored
        assert_eq!(load_history(&path), vec!["first", "second"]);

        // Consecutive duplicates collapse on load.
        append_history(&path, "second");
        assert_eq!(load_history(&path), vec!["first", "second"]);

        // Cap keeps the most recent entries.
        for i in 0..(HISTORY_LIMIT + 10) {
            append_history(&path, &format!("entry-{i}"));
        }
        let loaded = load_history(&path);
        assert_eq!(loaded.len(), HISTORY_LIMIT);
        assert_eq!(
            loaded.last().unwrap(),
            &format!("entry-{}", HISTORY_LIMIT + 9)
        );
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        std::fs::write(&path, "not json\n{\"text\":\"ok\"}\n{\"other\":1}\n").unwrap();
        assert_eq!(load_history(&path), vec!["ok"]);
    }
}
