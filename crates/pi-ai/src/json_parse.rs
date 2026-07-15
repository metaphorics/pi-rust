//! Best-effort parsing for incrementally streamed tool arguments.

use serde_json::{Map, Value};

/// Parses a complete JSON value or salvages a partial top-level object by
/// closing unterminated strings and containers. Invalid trailing members are
/// dropped until the longest valid object prefix is found.
pub fn parse_streaming_json(input: &str) -> Value {
    if let Ok(value) = serde_json::from_str(input) {
        return value;
    }
    let trimmed = input.trim();
    if !trimmed.starts_with('{') {
        return Value::Object(Map::new());
    }

    let mut candidate = trimmed.to_owned();
    let mut in_string = false;
    let mut escaped = false;
    let mut braces = 0_i32;
    let mut brackets = 0_i32;
    for ch in candidate.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                '{' => braces += 1,
                '}' => braces -= 1,
                '[' => brackets += 1,
                ']' => brackets -= 1,
                _ => {}
            }
        }
    }
    if in_string {
        if escaped {
            candidate.pop();
        }
        candidate.push('"');
    }
    candidate.extend(std::iter::repeat_n(']', brackets.max(0) as usize));
    candidate.extend(std::iter::repeat_n('}', braces.max(0) as usize));
    if let Ok(value) = serde_json::from_str(&candidate) {
        return value;
    }

    // A provider may stop between a key and its value. Repeatedly discard the
    // final member while preserving all already-complete tool arguments.
    let mut prefix = trimmed.to_owned();
    while let Some(comma) = prefix.rfind(',') {
        prefix.truncate(comma);
        let completed = format!("{prefix}}}");
        if let Ok(value) = serde_json::from_str(&completed) {
            return value;
        }
    }
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn salvages_partial_tool_arguments() {
        assert_eq!(
            parse_streaming_json(r#"{"path":"src/ma"#),
            json!({"path":"src/ma"})
        );
        assert_eq!(
            parse_streaming_json(r#"{"ok":true,"next": "#),
            json!({"ok":true})
        );
    }
}
