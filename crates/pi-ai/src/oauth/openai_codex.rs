use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_BASE_URL: &str = "https://auth.openai.com";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

pub async fn refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?
        .error_for_status()?;
    let token: TokenResponse = response.json().await?;
    credentials_from_token(
        token.access_token,
        token.refresh_token,
        jiff::Timestamp::now().as_millisecond() + token.expires_in * 1000,
    )
}

pub fn credentials_from_token(
    access: String,
    refresh: String,
    expires: i64,
) -> Result<OAuthCredential, OAuthError> {
    let payload = access
        .split('.')
        .nth(1)
        .ok_or_else(|| OAuthError::InvalidResponse("access token is not a JWT".into()))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| OAuthError::InvalidResponse(format!("invalid JWT payload: {error}")))?;
    let claims: Value = serde_json::from_slice(&decoded)
        .map_err(|error| OAuthError::InvalidResponse(format!("invalid JWT claims: {error}")))?;
    let account_id = claims
        .get(JWT_CLAIM_PATH)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            OAuthError::InvalidResponse("Failed to extract accountId from token".into())
        })?;
    let mut extra = std::collections::HashMap::new();
    extra.insert("accountId".into(), Value::String(account_id.into()));
    Ok(OAuthCredential {
        access,
        refresh,
        expires,
        extra,
    })
}

pub fn to_auth(credential: &OAuthCredential) -> ModelAuth {
    let headers = credential
        .extra
        .get("accountId")
        .and_then(Value::as_str)
        .map(|account_id| {
            [(
                "chatgpt-account-id".into(),
                Some(account_id.to_owned()),
            )]
            .into_iter()
            .collect()
        });
    ModelAuth {
        api_key: Some(credential.access.clone()),
        headers,
        base_url: None,
    }
}

pub struct OpenAICodexOAuth {
    client: reqwest::Client,
}

impl OpenAICodexOAuth {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for OpenAICodexOAuth {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

#[async_trait]
impl OAuthAuth for OpenAICodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
    }

    async fn refresh(&self, credential: &OAuthCredential) -> Result<OAuthCredential, OAuthError> {
        refresh_token(&self.client, &credential.refresh).await
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, OAuthError> {
        Ok(to_auth(credential))
    }
}
