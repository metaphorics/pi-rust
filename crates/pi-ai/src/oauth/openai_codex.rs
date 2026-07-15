use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::Value;

use super::{
    browser,
    callback_server::{CallbackServerConfig, start_callback_server},
    device_code::{DeviceCodePollOptions, poll_oauth_device_code_flow},
    pkce::{generate_pkce, generate_state},
    types::{
        DeviceCodePoll, OAuthDeviceCodeInfo, OAuthLoginCallbacks, OAuthPrompt, OAuthProvider,
        OAuthSelectOption, OAuthSelectPrompt, parse_authorization_input,
    },
};
use crate::auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_BASE_URL: &str = "https://auth.openai.com";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
pub const OPENAI_CODEX_BROWSER_LOGIN_METHOD: &str = "browser";
pub const OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
pub const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

#[derive(Clone, Debug)]
pub struct AuthorizationFlow {
    pub verifier: String,
    pub state: String,
    pub url: String,
}

/// Pure: build browser PKCE authorize URL.
pub fn create_authorization_flow(originator: &str) -> AuthorizationFlow {
    let pkce = generate_pkce();
    let state = generate_state_hex();
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is valid");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", originator);
    AuthorizationFlow {
        verifier: pkce.verifier,
        state,
        url: url.into(),
    }
}

fn generate_state_hex() -> String {
    // pi uses randomBytes(16).toString("hex")
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn exchange_authorization_code(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
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
            [("chatgpt-account-id".into(), Some(account_id.to_owned()))]
                .into_iter()
                .collect()
        });
    ModelAuth {
        api_key: Some(credential.access.clone()),
        headers,
        base_url: None,
    }
}

#[derive(Clone, Debug)]
pub struct DeviceAuthInfo {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct DeviceTokenSuccess {
    pub authorization_code: String,
    pub code_verifier: String,
}

pub async fn start_device_auth(
    client: &reqwest::Client,
    user_code_url: &str,
) -> Result<DeviceAuthInfo, OAuthError> {
    let response = client
        .post(user_code_url)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await?;
    if response.status().as_u16() == 404 {
        return Err(OAuthError::Other(
            "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL."
                .into(),
        ));
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::Other(format!(
            "OpenAI Codex device code request failed with status {status}{suffix}",
            suffix = if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            }
        )));
    }
    let json: Value = response.json().await?;
    let device_auth_id = json
        .get("device_auth_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OAuthError::InvalidResponse(format!(
                "Invalid OpenAI Codex device code response: {json}"
            ))
        })?;
    let user_code = json
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OAuthError::InvalidResponse(format!(
                "Invalid OpenAI Codex device code response: {json}"
            ))
        })?;
    let interval_seconds = match json.get("interval") {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| {
        OAuthError::InvalidResponse(format!("Invalid OpenAI Codex device code response: {json}"))
    })?;
    Ok(DeviceAuthInfo {
        device_auth_id: device_auth_id.into(),
        user_code: user_code.into(),
        interval_seconds,
    })
}

pub async fn poll_device_auth(
    client: &reqwest::Client,
    device_token_url: &str,
    device: &DeviceAuthInfo,
    cancellation: Option<super::device_code::CancellationFlag>,
) -> Result<DeviceTokenSuccess, OAuthError> {
    let device = device.clone();
    let client = client.clone();
    let device_token_url = device_token_url.to_owned();
    poll_oauth_device_code_flow(
        DeviceCodePollOptions {
            interval: Some(Duration::from_secs(device.interval_seconds)),
            expires_in: Some(Duration::from_secs(DEVICE_CODE_TIMEOUT_SECONDS)),
            wait_before_first_poll: false,
            cancellation,
        },
        || {
            let client = client.clone();
            let device_token_url = device_token_url.clone();
            let device = device.clone();
            async move {
                let response = client
                    .post(&device_token_url)
                    .json(&serde_json::json!({
                        "device_auth_id": device.device_auth_id,
                        "user_code": device.user_code,
                    }))
                    .send()
                    .await?;
                if response.status().is_success() {
                    let json: Value = response.json().await?;
                    let authorization_code = json
                        .get("authorization_code")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let code_verifier = json
                        .get("code_verifier")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    return match (authorization_code, code_verifier) {
                        (Some(authorization_code), Some(code_verifier)) => {
                            Ok(DeviceCodePoll::Complete(DeviceTokenSuccess {
                                authorization_code,
                                code_verifier,
                            }))
                        }
                        _ => Ok(DeviceCodePoll::Failed(format!(
                            "Invalid OpenAI Codex device auth token response: {json}"
                        ))),
                    };
                }
                let status = response.status().as_u16();
                if status == 403 || status == 404 {
                    return Ok(DeviceCodePoll::Pending);
                }
                let body = response.text().await.unwrap_or_default();
                let error_code = serde_json::from_str::<Value>(&body).ok().and_then(|json| {
                    json.get("error").and_then(|error| match error {
                        Value::String(s) => Some(s.clone()),
                        Value::Object(obj) => {
                            obj.get("code").and_then(Value::as_str).map(str::to_owned)
                        }
                        _ => None,
                    })
                });
                if error_code.as_deref() == Some("deviceauth_authorization_pending") {
                    return Ok(DeviceCodePoll::Pending);
                }
                if error_code.as_deref() == Some("slow_down") {
                    return Ok(DeviceCodePoll::SlowDown {
                        interval_seconds: None,
                    });
                }
                Ok(DeviceCodePoll::Failed(format!(
                    "OpenAI Codex device auth failed with status {status}{}",
                    if body.is_empty() {
                        String::new()
                    } else {
                        format!(": {body}")
                    }
                )))
            }
        },
    )
    .await
}

pub struct OpenAICodexOAuth {
    client: reqwest::Client,
    token_url: String,
    device_user_code_url: String,
    device_token_url: String,
    test_callback: Option<String>,
}

impl OpenAICodexOAuth {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            token_url: TOKEN_URL.into(),
            device_user_code_url: DEVICE_USER_CODE_URL.into(),
            device_token_url: DEVICE_TOKEN_URL.into(),
            test_callback: None,
        }
    }

    pub fn with_endpoints(
        client: reqwest::Client,
        token_url: impl Into<String>,
        device_user_code_url: impl Into<String>,
        device_token_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            token_url: token_url.into(),
            device_user_code_url: device_user_code_url.into(),
            device_token_url: device_token_url.into(),
            test_callback: None,
        }
    }

    pub fn with_test_callback(mut self, code: impl Into<String>) -> Self {
        self.test_callback = Some(code.into());
        self
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
        refresh_token_at(&self.client, &self.token_url, &credential.refresh).await
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, OAuthError> {
        Ok(to_auth(credential))
    }
}

#[async_trait]
impl OAuthProvider for OpenAICodexOAuth {
    fn id(&self) -> &str {
        "openai-codex"
    }

    fn uses_callback_server(&self) -> bool {
        true
    }

    async fn login(&self, callbacks: &OAuthLoginCallbacks) -> Result<OAuthCredential, OAuthError> {
        let method = (callbacks.on_select)(OAuthSelectPrompt {
            message: "Select OpenAI Codex login method:".into(),
            options: vec![
                OAuthSelectOption {
                    id: OPENAI_CODEX_BROWSER_LOGIN_METHOD.into(),
                    label: "Browser login (default)".into(),
                },
                OAuthSelectOption {
                    id: OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.into(),
                    label: "Device code login (headless)".into(),
                },
            ],
        })
        .await
        .ok_or_else(|| OAuthError::Other("Login cancelled".into()))?;

        if method == OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD {
            return self.login_device_code(callbacks).await;
        }
        if method != OPENAI_CODEX_BROWSER_LOGIN_METHOD {
            return Err(OAuthError::Other(format!(
                "Unknown OpenAI Codex login method: {method}"
            )));
        }
        self.login_browser(callbacks).await
    }
}

impl OpenAICodexOAuth {
    async fn login_browser(
        &self,
        callbacks: &OAuthLoginCallbacks,
    ) -> Result<OAuthCredential, OAuthError> {
        let flow = create_authorization_flow("pi");
        (callbacks.on_auth)(super::types::OAuthAuthInfo {
            url: flow.url.clone(),
            instructions: Some("A browser window should open. Complete login to finish.".into()),
        });
        if callbacks.open_browser {
            browser::open_url(&flow.url);
        }

        let code = if let Some(code) = &self.test_callback {
            code.clone()
        } else {
            collect_codex_code(callbacks, &flow.state).await?
        };

        exchange_authorization_code(
            &self.client,
            &self.token_url,
            &code,
            &flow.verifier,
            REDIRECT_URI,
        )
        .await
    }

    async fn login_device_code(
        &self,
        callbacks: &OAuthLoginCallbacks,
    ) -> Result<OAuthCredential, OAuthError> {
        let device = start_device_auth(&self.client, &self.device_user_code_url).await?;
        (callbacks.on_device_code)(OAuthDeviceCodeInfo {
            user_code: device.user_code.clone(),
            verification_uri: DEVICE_VERIFICATION_URI.into(),
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
        });
        let code = poll_device_auth(
            &self.client,
            &self.device_token_url,
            &device,
            callbacks.cancellation.clone(),
        )
        .await?;
        exchange_authorization_code(
            &self.client,
            &self.token_url,
            &code.authorization_code,
            &code.code_verifier,
            DEVICE_REDIRECT_URI,
        )
        .await
    }
}

async fn collect_codex_code(
    callbacks: &OAuthLoginCallbacks,
    expected_state: &str,
) -> Result<String, OAuthError> {
    let host = std::env::var("PI_OAUTH_CALLBACK_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    // Oracle (openai-codex.ts startLocalOAuthServer): bind failure soft-fails —
    // resolve a no-op server whose waitForCode yields null, then fall through
    // to onManualCodeInput / onPrompt paste path.
    let server = start_callback_server(CallbackServerConfig {
        host,
        port: 1455,
        path: "/auth/callback".into(),
        expected_state: Some(expected_state.to_owned()),
        success_message: "OpenAI authentication completed. You can close this window.".into(),
    })
    .await
    .ok();

    let mut code: Option<String> = None;
    if let Some(server) = server {
        if let Some(manual) = &callbacks.on_manual_code_input {
            tokio::select! {
                result = server.wait_for_code() => {
                    if let Ok(Some(cb)) = result {
                        code = Some(cb.code);
                    }
                }
                input = manual() => {
                    let (c, s) = parse_authorization_input(&input);
                    if let Some(s) = &s
                        && s.as_str() != expected_state
                    {
                        return Err(OAuthError::Other("State mismatch".into()));
                    }
                    code = c;
                }
            }
        } else if let Ok(Some(cb)) = server.wait_for_code().await {
            code = Some(cb.code);
        }
    } else if let Some(manual) = &callbacks.on_manual_code_input {
        // No callback server: still honor an immediate manual-input race.
        let input = manual().await;
        let (c, s) = parse_authorization_input(&input);
        if let Some(s) = &s
            && s.as_str() != expected_state
        {
            return Err(OAuthError::Other("State mismatch".into()));
        }
        code = c;
    }

    if code.is_none() {
        let input = (callbacks.on_prompt)(OAuthPrompt {
            message: "Paste the authorization code (or full redirect URL):".into(),
            placeholder: None,
            allow_empty: None,
        })
        .await;
        let (c, s) = parse_authorization_input(&input);
        if let Some(s) = &s
            && s.as_str() != expected_state
        {
            return Err(OAuthError::Other("State mismatch".into()));
        }
        code = c;
    }

    code.ok_or_else(|| OAuthError::Other("Missing authorization code".into()))
}

// Silence unused import when generate_state is only used via generate_state_hex.
#[allow(dead_code)]
fn _use_generate_state() {
    let _ = generate_state();
}
