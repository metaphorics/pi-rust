use async_trait::async_trait;
use serde::Deserialize;

use super::{
    browser,
    callback_server::{CallbackServerConfig, start_callback_server},
    pkce::generate_pkce,
    types::{OAuthLoginCallbacks, OAuthPrompt, OAuthProvider, parse_authorization_input},
};
use crate::auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CALLBACK_PORT: u16 = 53692;
pub const CALLBACK_PATH: &str = "/callback";
pub const REDIRECT_URI: &str = "http://localhost:53692/callback";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

/// Pure: build the Anthropic authorize URL (state == PKCE verifier, matching pi).
pub fn build_authorize_url(challenge: &str, state: &str) -> String {
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is valid");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.into()
}

pub async fn exchange_authorization_code(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    state: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(token_url)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
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

pub async fn refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<OAuthCredential, OAuthError> {
    refresh_token_at(client, TOKEN_URL, refresh_token).await
}

pub async fn refresh_token_at(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(token_url)
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
    token_url: String,
    /// When set, login skips the real callback bind and uses this code+state.
    test_callback: Option<(String, String)>,
}

impl AnthropicOAuth {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            token_url: TOKEN_URL.into(),
            test_callback: None,
        }
    }

    pub fn with_token_url(client: reqwest::Client, token_url: impl Into<String>) -> Self {
        Self {
            client,
            token_url: token_url.into(),
            test_callback: None,
        }
    }

    /// Test-only: inject the authorization code without binding a port.
    pub fn with_test_callback(mut self, code: impl Into<String>, state: impl Into<String>) -> Self {
        self.test_callback = Some((code.into(), state.into()));
        self
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
        refresh_token_at(&self.client, &self.token_url, &credential.refresh).await
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, OAuthError> {
        Ok(to_auth(credential))
    }
}

#[async_trait]
impl OAuthProvider for AnthropicOAuth {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn uses_callback_server(&self) -> bool {
        true
    }

    async fn login(&self, callbacks: &OAuthLoginCallbacks) -> Result<OAuthCredential, OAuthError> {
        let pkce = generate_pkce();
        // pi uses the PKCE verifier as the OAuth `state` parameter.
        let state = pkce.verifier.clone();
        let auth_url = build_authorize_url(&pkce.challenge, &state);

        (callbacks.on_auth)(super::types::OAuthAuthInfo {
            url: auth_url.clone(),
            instructions: Some(
                "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .into(),
            ),
        });

        if callbacks.open_browser {
            browser::open_url(&auth_url);
        }

        let (code, state_for_exchange) = if let Some((code, st)) = &self.test_callback {
            (code.clone(), st.clone())
        } else {
            collect_anthropic_code(callbacks, &state).await?
        };

        callbacks.progress("Exchanging authorization code for tokens...");
        exchange_authorization_code(
            &self.client,
            &self.token_url,
            &code,
            &state_for_exchange,
            &pkce.verifier,
            REDIRECT_URI,
        )
        .await
    }
}

async fn collect_anthropic_code(
    callbacks: &OAuthLoginCallbacks,
    expected_state: &str,
) -> Result<(String, String), OAuthError> {
    let server = start_callback_server(CallbackServerConfig {
        host: std::env::var("PI_OAUTH_CALLBACK_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: CALLBACK_PORT,
        path: CALLBACK_PATH.into(),
        expected_state: Some(expected_state.to_owned()),
        success_message: "Anthropic authentication completed. You can close this window.".into(),
    })
    .await?;

    let mut code: Option<String> = None;
    let mut state_out: Option<String> = None;

    if let Some(manual) = &callbacks.on_manual_code_input {
        tokio::select! {
            result = server.wait_for_code() => {
                if let Ok(Some(cb)) = result {
                    code = Some(cb.code);
                    state_out = cb.state.or_else(|| Some(expected_state.to_owned()));
                }
            }
            input = manual() => {
                // Dropping server cancels the wait.
                let (c, s) = parse_authorization_input(&input);
                if let Some(s) = &s
                    && s.as_str() != expected_state
                {
                    return Err(OAuthError::Other("OAuth state mismatch".into()));
                }
                code = c;
                state_out = s.or_else(|| Some(expected_state.to_owned()));
            }
        }
    } else if let Ok(Some(cb)) = server.wait_for_code().await {
        code = Some(cb.code);
        state_out = cb.state.or_else(|| Some(expected_state.to_owned()));
    }

    if code.is_none() {
        let input = (callbacks.on_prompt)(OAuthPrompt {
            message: "Paste the authorization code or full redirect URL:".into(),
            placeholder: Some(REDIRECT_URI.into()),
            allow_empty: None,
        })
        .await;
        let (c, s) = parse_authorization_input(&input);
        if let Some(s) = &s
            && s.as_str() != expected_state
        {
            return Err(OAuthError::Other("OAuth state mismatch".into()));
        }
        code = c;
        state_out = s.or_else(|| Some(expected_state.to_owned()));
    }

    let code = code.ok_or_else(|| OAuthError::Other("Missing authorization code".into()))?;
    let state_out = state_out.ok_or_else(|| OAuthError::Other("Missing OAuth state".into()))?;
    Ok((code, state_out))
}
