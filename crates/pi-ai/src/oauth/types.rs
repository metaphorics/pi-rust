use std::{future::Future, pin::Pin, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::auth::{ModelAuth, OAuthAuth, OAuthCredential};
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthPrompt {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_empty: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSelectOption {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSelectPrompt {
    pub message: String,
    pub options: Vec<OAuthSelectOption>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceCodePoll<T> {
    Pending,
    SlowDown { interval_seconds: Option<u64> },
    Complete(T),
    Failed(String),
}

pub type PromptFuture = Pin<Box<dyn Future<Output = String> + Send>>;
pub type PromptCallback = Box<dyn Fn(OAuthPrompt) -> PromptFuture + Send + Sync>;
pub type ManualCodeCallback = Box<dyn Fn() -> PromptFuture + Send + Sync>;
pub type SelectFuture = Pin<Box<dyn Future<Output = Option<String>> + Send>>;
pub type SelectCallback = Box<dyn Fn(OAuthSelectPrompt) -> SelectFuture + Send + Sync>;
pub type ProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Login-flow callbacks (mirrors `OAuthLoginCallbacks` in pi).
pub struct OAuthLoginCallbacks {
    pub on_auth: Box<dyn Fn(OAuthAuthInfo) + Send + Sync>,
    pub on_device_code: Box<dyn Fn(OAuthDeviceCodeInfo) + Send + Sync>,
    pub on_prompt: PromptCallback,
    pub on_progress: Option<ProgressCallback>,
    pub on_manual_code_input: Option<ManualCodeCallback>,
    pub on_select: SelectCallback,
    pub cancellation: Option<super::device_code::CancellationFlag>,
    /// When false, browser open is skipped (tests inject the callback hit).
    pub open_browser: bool,
}

impl OAuthLoginCallbacks {
    pub fn progress(&self, message: &str) {
        if let Some(cb) = &self.on_progress {
            cb(message);
        }
    }
}

/// Full OAuth provider surface: login + refresh + to_auth (mirrors `OAuthProviderInterface`).
#[async_trait]
pub trait OAuthProvider: OAuthAuth {
    fn id(&self) -> &str;
    fn uses_callback_server(&self) -> bool {
        false
    }
    async fn login(&self, callbacks: &OAuthLoginCallbacks) -> Result<OAuthCredential, OAuthError>;
    fn get_api_key(&self, credentials: &OAuthCredential) -> String {
        credentials.access.clone()
    }
}

pub type SharedOAuthProvider = Arc<dyn OAuthProvider>;

/// Helper to convert an OAuthAuth-only type into ModelAuth via to_auth.
pub async fn provider_to_auth(
    provider: &dyn OAuthAuth,
    credential: &OAuthCredential,
) -> Result<ModelAuth, OAuthError> {
    provider.to_auth(credential).await
}

/// Parse authorization code / redirect URL input (shared by Anthropic + Codex).
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }

    if let Ok(url) = url::Url::parse(value) {
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned());
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned());
        return (code, state);
    }

    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_owned()), Some(state.to_owned()));
    }

    if value.contains("code=") {
        let params: std::collections::HashMap<_, _> = url::form_urlencoded::parse(value.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        return (params.get("code").cloned(), params.get("state").cloned());
    }

    (Some(value.to_owned()), None)
}
