use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::types::{InstanceRecord, InstanceStatus};
use crate::wire::RpcCommandEnvelope;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OrchestratorRequest {
    #[serde(rename = "spawn")]
    Spawn {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    #[serde(rename = "list")]
    List,
    #[serde(rename = "stop")]
    Stop {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
    #[serde(rename = "status")]
    Status {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
    #[serde(rename = "rpc")]
    Rpc {
        #[serde(rename = "instanceId")]
        instance_id: String,
        #[serde(
            serialize_with = "serialize_command",
            deserialize_with = "deserialize_command"
        )]
        command: RpcCommandEnvelope,
    },
    #[serde(rename = "rpc_stream")]
    RpcStream {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
}

impl OrchestratorRequest {
    pub fn rpc_stream_instance_id(&self) -> Option<&str> {
        match self {
            Self::RpcStream { instance_id } => Some(instance_id),
            _ => None,
        }
    }
}

fn serialize_command<S>(command: &RpcCommandEnvelope, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    command.raw.serialize(serializer)
}

fn deserialize_command<'de, D>(deserializer: D) -> Result<RpcCommandEnvelope, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    RpcCommandEnvelope::try_from(value).map_err(serde::de::Error::custom)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSummary {
    pub id: String,
    pub status: InstanceStatus,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radius_pi_id: Option<String>,
}

impl From<&InstanceRecord> for InstanceSummary {
    fn from(record: &InstanceRecord) -> Self {
        Self {
            id: record.id.clone(),
            status: record.status,
            cwd: record.cwd.clone(),
            label: record.label.clone(),
            session_id: record.session_id.clone(),
            session_file: record.session_file.clone(),
            radius_pi_id: record.radius_pi_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OrchestratorResponse {
    #[serde(rename = "spawn_result")]
    SpawnResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceSummary>,
    },
    #[serde(rename = "list_result")]
    ListResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instances: Option<Vec<InstanceSummary>>,
    },
    #[serde(rename = "stop_result")]
    StopResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(rename = "instanceId", skip_serializing_if = "Option::is_none")]
        instance_id: Option<String>,
    },
    #[serde(rename = "status_result")]
    StatusResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceSummary>,
    },
    #[serde(rename = "rpc_result")]
    RpcResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        response: Value,
    },
    #[serde(rename = "rpc_ready")]
    RpcReady {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceSummary>,
    },
    #[serde(rename = "error")]
    Error { ok: bool, error: String },
}

impl OrchestratorResponse {
    pub fn error(error: impl Into<String>) -> Self {
        Self::Error {
            ok: false,
            error: error.into(),
        }
    }

    pub fn rpc_ready_instance(&self) -> Option<&InstanceSummary> {
        match self {
            Self::RpcReady {
                ok: true,
                instance: Some(instance),
                ..
            } => Some(instance),
            _ => None,
        }
    }
}

pub type ProtocolError = serde_json::Error;

pub fn encode_message<T>(message: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    Ok(line)
}

pub fn parse_request_line(line: &str) -> Result<OrchestratorRequest, ProtocolError> {
    serde_json::from_str(line)
}

pub fn parse_response_line(line: &str) -> Result<OrchestratorResponse, ProtocolError> {
    serde_json::from_str(line)
}
