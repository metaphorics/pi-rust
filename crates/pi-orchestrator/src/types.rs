use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Starting,
    Online,
    Stopping,
    Stopped,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineRecord {
    pub id: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusRegistration {
    pub heartbeat_interval_ms: u64,
    pub expires_in_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    pub id: String,
    pub status: InstanceStatus,
    pub cwd: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radius_pi_id: Option<String>,
}

/// Return the current UTC time in the exact format produced by `Date#toISOString`.
pub fn now_iso_timestamp() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_json_uses_oracle_order_and_omits_absent_fields() {
        let record = InstanceRecord {
            id: "worker-1".into(),
            status: InstanceStatus::Online,
            cwd: "/work".into(),
            created_at: "2025-12-09T00:53:29.825Z".into(),
            last_seen_at: Some("2025-12-09T00:54:00.001Z".into()),
            label: None,
            session_id: Some("session-1".into()),
            session_file: None,
            radius_pi_id: Some("radius-1".into()),
        };

        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"id":"worker-1","status":"online","cwd":"/work","createdAt":"2025-12-09T00:53:29.825Z","lastSeenAt":"2025-12-09T00:54:00.001Z","sessionId":"session-1","radiusPiId":"radius-1"}"#
        );
    }

    #[test]
    fn machine_json_uses_oracle_order_and_omits_absent_fields() {
        let record = MachineRecord {
            id: "machine-1".into(),
            created_at: "2025-12-09T00:53:29.825Z".into(),
            last_seen_at: None,
            label: Some("desk".into()),
        };

        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"id":"machine-1","createdAt":"2025-12-09T00:53:29.825Z","label":"desk"}"#
        );
    }

    #[test]
    fn generated_timestamp_has_fixed_millisecond_precision() {
        let timestamp = now_iso_timestamp();
        let bytes = timestamp.as_bytes();
        assert_eq!(bytes.len(), 24, "unexpected timestamp: {timestamp}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert_eq!(bytes[10], b'T');
        assert_eq!(bytes[13], b':');
        assert_eq!(bytes[16], b':');
        assert_eq!(bytes[19], b'.');
        assert_eq!(bytes[23], b'Z');
        assert!(bytes[20..23].iter().all(u8::is_ascii_digit));
        timestamp.parse::<jiff::Timestamp>().unwrap();
    }
}
