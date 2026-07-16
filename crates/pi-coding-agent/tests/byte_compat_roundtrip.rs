//! BYTE-COMPAT golden: parse → reserialize → assert identical bytes.
//!
//! Fixtures:
//! - `session_pi_written.jsonl` — excerpt from pi's own before-compaction fixture
//! - `session_v3_all_kinds.jsonl` — synthetic v3 covering every entry kind
//! - `settings_sample.json` / `auth_sample.json` — ordered JSON documents

use pi_coding_agent::{
    FileEntry, parse_session_entry_line, parse_settings_json, serialize_file_entry_line,
    serialize_settings_json,
};
use serde_json::Value;

fn assert_line_roundtrip(line: &str) {
    let entry: FileEntry =
        parse_session_entry_line(line).unwrap_or_else(|| panic!("parse failed: {line}"));
    let out = serialize_file_entry_line(&entry).expect("serialize");
    assert_eq!(out, line, "byte mismatch\n left: {out}\nright: {line}");
}

#[test]
fn session_pi_written_lines_roundtrip_byte_identical() {
    let source = include_str!("fixtures/session_pi_written.jsonl");
    let mut count = 0usize;
    for line in source.lines() {
        if line.trim().is_empty() {
            continue;
        }
        assert_line_roundtrip(line);
        count += 1;
    }
    assert!(count >= 6, "expected multi-kind sample, got {count}");
}

#[test]
fn session_pi_written_full_file_roundtrip() {
    let source = include_str!("fixtures/session_pi_written.jsonl");
    let mut entries = Vec::new();
    for line in source.lines() {
        if let Some(e) = parse_session_entry_line(line) {
            entries.push(e);
        }
    }
    let mut out = String::new();
    for e in &entries {
        out.push_str(&serialize_file_entry_line(e).unwrap());
        out.push('\n');
    }
    assert_eq!(out, source);
}

#[test]
fn session_v3_all_kinds_roundtrip_byte_identical() {
    let source = include_str!("fixtures/session_v3_all_kinds.jsonl");
    // Custom lines mirror the object-literal insertion order in pi's
    // appendCustomEntry and appendCustomMessageEntry implementations.
    for line in source.lines().filter(|l| !l.trim().is_empty()) {
        assert_line_roundtrip(line);
    }
    let kinds: Vec<String> = source
        .lines()
        .filter_map(|l| {
            let v: Value = serde_json::from_str(l).ok()?;
            v.get("type")?.as_str().map(str::to_string)
        })
        .collect();
    for expected in [
        "session",
        "message",
        "thinking_level_change",
        "model_change",
        "compaction",
        "branch_summary",
        "custom",
        "custom_message",
        "label",
        "session_info",
    ] {
        assert!(
            kinds.iter().any(|k| k == expected),
            "missing kind {expected} in {kinds:?}"
        );
    }
}

#[test]
fn settings_sample_roundtrip_byte_identical() {
    let source = include_str!("fixtures/settings_sample.json");
    let settings = parse_settings_json(source).expect("parse settings");
    let out = serialize_settings_json(&settings);
    assert_eq!(out, source);
}

#[test]
fn auth_sample_roundtrip_byte_identical() {
    let source = include_str!("fixtures/auth_sample.json");
    let raw: Value = serde_json::from_str(source).expect("parse auth document");
    let typed = raw
        .as_object()
        .expect("auth document is an object")
        .iter()
        .map(|(provider, value)| {
            let credential: pi_ai::auth::Credential =
                serde_json::from_value(value.clone()).expect("parse typed credential");
            let value = serde_json::to_value(credential).expect("serialize typed credential");
            (provider.clone(), value)
        })
        .collect();
    let out = serde_json::to_string_pretty(&Value::Object(typed)).expect("serialize auth");
    assert_eq!(out, source);
}

#[test]
fn settings_migration_does_not_change_current_file() {
    let source = include_str!("fixtures/settings_sample.json");
    let s1 = parse_settings_json(source).unwrap();
    let s2 = parse_settings_json(&serialize_settings_json(&s1)).unwrap();
    assert_eq!(serialize_settings_json(&s1), serialize_settings_json(&s2));
}
