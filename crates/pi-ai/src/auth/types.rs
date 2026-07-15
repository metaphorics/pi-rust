use std::{collections::HashMap, future::Future, pin::Pin};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{ProviderEnv, ProviderHeaders};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthResult {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    pub source: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
    #[error("credential storage I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential storage JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("credential update failed: {0}")]
    Update(String),
}

pub type CredentialFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Credential>, CredentialStoreError>> + Send + 'a>>;
pub type CredentialModifier<'a> =
    Box<dyn FnOnce(Option<Credential>) -> CredentialFuture<'a> + Send + 'a>;

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn read(&self, provider_id: &str) -> Result<Option<Credential>, CredentialStoreError>;

    async fn modify<'a>(
        &'a self,
        provider_id: &str,
        modifier: CredentialModifier<'a>,
    ) -> Result<Option<Credential>, CredentialStoreError>;

    async fn delete(&self, provider_id: &str) -> Result<(), CredentialStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("OAuth HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OAuth response was invalid: {0}")]
    InvalidResponse(String),
    #[error("OAuth operation failed: {0}")]
    Other(String),
}

#[async_trait]
pub trait OAuthAuth: Send + Sync {
    fn name(&self) -> &str;
    async fn refresh(&self, credential: &OAuthCredential) -> Result<OAuthCredential, OAuthError>;
    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, OAuthError>;
}
