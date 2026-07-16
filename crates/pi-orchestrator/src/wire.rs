use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const SESSION_METADATA_COMMANDS: [&str; 6] = [
    "new_session",
    "switch_session",
    "fork",
    "clone",
    "set_session_name",
    "prompt",
];

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("expected a JSON object")]
    ExpectedObject,
    #[error("missing or invalid string field `{0}`")]
    InvalidStringField(&'static str),
    #[error("missing or invalid boolean field `{0}`")]
    InvalidBooleanField(&'static str),
}

pub type Result<T, E = WireError> = std::result::Result<T, E>;

/// The command fields inspected by the orchestrator plus the original object.
///
/// `raw` retains all unknown fields and their insertion order for forwarding.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcCommandEnvelope {
    pub id: Option<String>,
    pub kind: String,
    pub raw: Map<String, Value>,
}

impl RpcCommandEnvelope {
    pub fn refreshes_session_metadata(&self) -> bool {
        SESSION_METADATA_COMMANDS.contains(&self.kind.as_str())
    }

    /// Insert a generated id using JavaScript object-spread ordering semantics.
    ///
    /// A missing id is appended. An existing (or explicit null) id retains its
    /// original position because replacing an object property does not move it.
    pub fn into_value_with_generated_id(
        mut self,
        generate_id: impl FnOnce() -> String,
    ) -> (String, Value) {
        let id = self.id.take().unwrap_or_else(generate_id);
        self.raw.insert("id".into(), Value::String(id.clone()));
        (id, Value::Object(self.raw))
    }
}

impl TryFrom<Value> for RpcCommandEnvelope {
    type Error = WireError;

    fn try_from(value: Value) -> Result<Self> {
        let Value::Object(raw) = value else {
            return Err(WireError::ExpectedObject);
        };
        let kind = required_string(&raw, "type")?.to_owned();
        let id = optional_string(&raw, "id")?.map(str::to_owned);
        Ok(Self { id, kind, raw })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RpcResponseEnvelope {
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    pub raw: Value,
}

impl RpcResponseEnvelope {
    pub fn get_state(&self) -> Result<Option<GetStateData>> {
        if !self.success || self.command != "get_state" {
            return Ok(None);
        }
        let Some(data) = self.raw.get("data") else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_value(data.clone())?))
    }
}

impl TryFrom<Value> for RpcResponseEnvelope {
    type Error = WireError;

    fn try_from(value: Value) -> Result<Self> {
        let Value::Object(raw) = &value else {
            return Err(WireError::ExpectedObject);
        };
        let id = optional_string(raw, "id")?.map(str::to_owned);
        let command = required_string(raw, "command")?.to_owned();
        let success = raw
            .get("success")
            .and_then(Value::as_bool)
            .ok_or(WireError::InvalidBooleanField("success"))?;
        Ok(Self {
            id,
            command,
            success,
            raw: value,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetStateData {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChildLine {
    Response(RpcResponseEnvelope),
    UiRequest(Value),
    Event(Value),
}

pub fn classify_child_line(line: &str) -> Result<ChildLine> {
    let value: Value = serde_json::from_str(line)?;
    match value.get("type").and_then(Value::as_str) {
        Some("response") => Ok(ChildLine::Response(RpcResponseEnvelope::try_from(value)?)),
        Some("extension_ui_request") => Ok(ChildLine::UiRequest(value)),
        _ => Ok(ChildLine::Event(value)),
    }
}

pub fn encode_line<T>(value: &T) -> Result<String>
where
    T: Serialize + ?Sized,
{
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

fn required_string<'a>(raw: &'a Map<String, Value>, field: &'static str) -> Result<&'a str> {
    raw.get(field)
        .and_then(Value::as_str)
        .ok_or(WireError::InvalidStringField(field))
}

fn optional_string<'a>(
    raw: &'a Map<String, Value>,
    field: &'static str,
) -> Result<Option<&'a str>> {
    match raw.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(WireError::InvalidStringField(field)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(kind: &str) -> RpcCommandEnvelope {
        RpcCommandEnvelope::try_from(json!({ "type": kind })).unwrap()
    }

    #[test]
    fn classifier_covers_response_ui_request_and_event_branches() {
        let response = classify_child_line(
            r#"{"type":"response","id":"one","command":"get_state","success":true,"data":{"sessionId":"s1"}}"#,
        )
        .unwrap();
        let ChildLine::Response(response) = response else {
            panic!("expected response")
        };
        assert_eq!(response.id.as_deref(), Some("one"));
        assert_eq!(response.command, "get_state");

        let ui =
            classify_child_line(r#"{"type":"extension_ui_request","id":"ui-1","method":"select"}"#)
                .unwrap();
        assert!(matches!(ui, ChildLine::UiRequest(_)));

        let event = classify_child_line(r#"{"type":"message_update","seq":1}"#).unwrap();
        assert!(matches!(event, ChildLine::Event(_)));
        let missing_type = classify_child_line(r#"{"seq":2}"#).unwrap();
        assert!(matches!(missing_type, ChildLine::Event(_)));
        let primitive = classify_child_line("42").unwrap();
        assert!(matches!(primitive, ChildLine::Event(Value::Number(_))));
    }

    #[test]
    fn metadata_commands_are_an_exact_closed_set() {
        for kind in SESSION_METADATA_COMMANDS {
            assert!(command(kind).refreshes_session_metadata(), "{kind}");
        }
        for kind in [
            "get_state",
            "abort",
            "compact",
            "prompt_extra",
            "new-session",
            "",
        ] {
            assert!(!command(kind).refreshes_session_metadata(), "{kind}");
        }
    }

    #[test]
    fn generated_id_is_appended_without_reordering_unknown_fields() {
        let command = RpcCommandEnvelope::try_from(
            serde_json::from_str::<Value>(
                r#"{"type":"prompt","message":"hello","options":{"stream":true}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let (id, value) = command.into_value_with_generated_id(|| {
            "orchestrator_1_00000000-0000-4000-8000-000000000000".into()
        });

        assert_eq!(id, "orchestrator_1_00000000-0000-4000-8000-000000000000");
        assert_eq!(
            encode_line(&value).unwrap(),
            "{\"type\":\"prompt\",\"message\":\"hello\",\"options\":{\"stream\":true},\"id\":\"orchestrator_1_00000000-0000-4000-8000-000000000000\"}\n"
        );
    }

    #[test]
    fn existing_or_null_id_keeps_its_property_position() {
        let existing = RpcCommandEnvelope::try_from(
            serde_json::from_str::<Value>(r#"{"type":"prompt","id":"client-1","message":"hi"}"#)
                .unwrap(),
        )
        .unwrap();
        let (id, value) = existing.into_value_with_generated_id(|| panic!("must not generate"));
        assert_eq!(id, "client-1");
        assert_eq!(
            encode_line(&value).unwrap(),
            "{\"type\":\"prompt\",\"id\":\"client-1\",\"message\":\"hi\"}\n"
        );

        let null_id = RpcCommandEnvelope::try_from(
            serde_json::from_str::<Value>(r#"{"type":"prompt","id":null,"message":"hi"}"#).unwrap(),
        )
        .unwrap();
        let (_, value) = null_id.into_value_with_generated_id(|| "generated".into());
        assert_eq!(
            encode_line(&value).unwrap(),
            "{\"type\":\"prompt\",\"id\":\"generated\",\"message\":\"hi\"}\n"
        );
    }

    #[test]
    fn get_state_view_parses_only_successful_get_state_responses() {
        let ChildLine::Response(response) = classify_child_line(
            r#"{"type":"response","id":"one","command":"get_state","success":true,"data":{"sessionId":"session-1","sessionFile":"/tmp/session.jsonl"}}"#,
        )
        .unwrap()
        else {
            panic!("expected response")
        };
        assert_eq!(
            response.get_state().unwrap(),
            Some(GetStateData {
                session_id: "session-1".into(),
                session_file: Some("/tmp/session.jsonl".into()),
            })
        );

        let failed = RpcResponseEnvelope::try_from(json!({
            "type": "response",
            "command": "get_state",
            "success": false,
            "data": { "sessionId": "ignored" }
        }))
        .unwrap();
        assert_eq!(failed.get_state().unwrap(), None);

        let other = RpcResponseEnvelope::try_from(json!({
            "type": "response",
            "command": "prompt",
            "success": true,
            "data": { "sessionId": "ignored" }
        }))
        .unwrap();
        assert_eq!(other.get_state().unwrap(), None);

        let missing_data = RpcResponseEnvelope::try_from(json!({
            "type": "response",
            "command": "get_state",
            "success": true
        }))
        .unwrap();
        assert_eq!(missing_data.get_state().unwrap(), None);
    }

    #[test]
    fn compact_line_encoder_has_one_newline() {
        assert_eq!(
            encode_line(&json!({ "type": "list", "nested": { "ok": true } })).unwrap(),
            "{\"type\":\"list\",\"nested\":{\"ok\":true}}\n"
        );
    }

    #[test]
    fn malformed_envelope_fields_are_rejected() {
        assert!(RpcCommandEnvelope::try_from(json!({ "type": 1 })).is_err());
        assert!(RpcCommandEnvelope::try_from(json!({ "type": "prompt", "id": 1 })).is_err());
        assert!(
            classify_child_line(r#"{"type":"response","command":"get_state","success":"yes"}"#)
                .is_err()
        );
    }
}
