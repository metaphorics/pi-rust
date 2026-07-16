//! Tool argument preparation and lightweight JSON-schema checks.

use pi_ai::ToolCall;
use serde_json::Value;

use crate::types::{AgentToolCall, AgentToolResult, ToolDefinition};

/// Prepare tool-call arguments via the tool's optional shim, then validate
/// against the tool's JSON Schema parameters.
///
/// Mirrors pi-ai `validateToolArguments` for the subset of JSON Schema used by
/// agent tests (object properties + required + basic type checks). Full AJV
/// coercion is intentionally not reimplemented here; Phase 5 may deepen this.
pub fn prepare_and_validate_arguments(
    tool: &ToolDefinition,
    tool_call: &AgentToolCall,
) -> Result<Value, String> {
    let prepared_call = prepare_tool_call_arguments(tool, tool_call);
    validate_tool_arguments(tool, &prepared_call)
}

pub fn prepare_tool_call_arguments(
    tool: &ToolDefinition,
    tool_call: &AgentToolCall,
) -> AgentToolCall {
    let Some(prepare) = tool.prepare_arguments.as_ref() else {
        return tool_call.clone();
    };
    let original = Value::Object(tool_call.arguments.clone());
    let prepared = prepare(original.clone());
    if prepared == original {
        return tool_call.clone();
    }
    let mut call = tool_call.clone();
    call.arguments = match prepared {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("_".into(), other);
            map
        }
    };
    call
}

pub fn validate_tool_arguments(
    tool: &ToolDefinition,
    tool_call: &ToolCall,
) -> Result<Value, String> {
    let args = Value::Object(tool_call.arguments.clone());
    if let Err(errors) = validate_value(&args, &tool.parameters, "$") {
        let joined = errors
            .into_iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{}",
            tool_call.name,
            joined,
            serde_json::to_string_pretty(&tool_call.arguments).unwrap_or_else(|_| "{}".into())
        ));
    }
    Ok(args)
}

fn validate_value(value: &Value, schema: &Value, path: &str) -> Result<(), Vec<String>> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };

    if let Some(types) = schema_obj.get("type") {
        let ok = match types {
            Value::String(expected) => type_matches(value, expected),
            Value::Array(options) => options
                .iter()
                .filter_map(Value::as_str)
                .any(|expected| type_matches(value, expected)),
            _ => true,
        };
        if !ok {
            return Err(vec![format!(
                "{path}: expected type {}, got {}",
                types,
                json_type_name(value)
            )]);
        }
    }

    if let Some(required) = schema_obj.get("required").and_then(Value::as_array)
        && let Some(obj) = value.as_object()
    {
        let mut errors = Vec::new();
        for key in required {
            let Some(name) = key.as_str() else {
                continue;
            };
            if !obj.contains_key(name) {
                errors.push(format!("{path}: missing required property `{name}`"));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
    }

    if let Some(properties) = schema_obj.get("properties").and_then(Value::as_object)
        && let Some(obj) = value.as_object()
    {
        let mut errors = Vec::new();
        for (key, prop_schema) in properties {
            if let Some(prop_value) = obj.get(key) {
                let child_path = if path == "$" {
                    format!("$.{key}")
                } else {
                    format!("{path}.{key}")
                };
                if let Err(mut child_errors) = validate_value(prop_value, prop_schema, &child_path)
                {
                    errors.append(&mut child_errors);
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
    }

    if let Some(items_schema) = schema_obj.get("items")
        && let Some(arr) = value.as_array()
    {
        let mut errors = Vec::new();
        for (index, item) in arr.iter().enumerate() {
            let child_path = format!("{path}[{index}]");
            if let Err(mut child_errors) = validate_value(item, items_schema, &child_path) {
                errors.append(&mut child_errors);
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
    }

    Ok(())
}

fn type_matches(value: &Value, expected: &str) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub fn error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult::error_text(message)
}
