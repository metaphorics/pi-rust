use async_trait::async_trait;
use serde::Deserialize;

use crate::auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CALLBACK_PORT: u16 = 53692;
pub const REDIRECT_URI: &str = "http://localhost:53692/callback";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

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
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await?
        .error_for_status()?;
    let token: TokenResponse = response.json().await?;
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token.refresh_token,
        expires: jiff::Timestamp::now().as_millisecond() + token.expires_in * 1000 - 300_000,
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

pub struct AnthropicOAuth {
    client: reqwest::Client,
}

impl AnthropicOAuth {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for AnthropicOAuth {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

#[async_trait]
impl OAuthAuth for AnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    async fn refresh(&self, credential: &OAuthCredential) -> Result<OAuthCredential, OAuthError> {
        refresh_token(&self.client, &credential.refresh).await
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, OAuthError> {
        Ok(to_auth(credential))
    }
}
