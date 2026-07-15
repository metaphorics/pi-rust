use async_trait::async_trait;
use serde::Deserialize;

use crate::auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_BASE_URL: &str = "https://auth.openai.com";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const SCOPE: &str = "openid profile email offline_access";

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
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token.refresh_token,
        expires: jiff::Timestamp::now().as_millisecond() + token.expires_in * 1000,
        extra: Default::default(),
    })
}

pub fn to_auth(credential: &OAuthCredential) -> ModelAuth {
    ModelAuth {
        api_key: Some(credential.access.clone()),
        headers: None,
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
