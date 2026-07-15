use serde::{Deserialize, Serialize};

pub use crate::auth::{OAuthCredential as OAuthCredentials, OAuthError};

pub type OAuthProviderId = String;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthAuthInfo {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthDeviceCodeInfo {
    pub user_code: String,
    pub verification_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceCodePoll<T> {
    Pending,
    SlowDown { interval_seconds: Option<u64> },
    Complete(T),
    Failed(String),
}
