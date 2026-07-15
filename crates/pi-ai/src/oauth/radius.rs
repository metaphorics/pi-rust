use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    browser,
    callback_server::{CallbackServerConfig, start_callback_server},
    device_code::{DeviceCodePollOptions, poll_oauth_device_code_flow},
    pkce::{generate_pkce, generate_state},
    types::{
        DeviceCodePoll, OAuthDeviceCodeInfo, OAuthLoginCallbacks, OAuthProvider, OAuthSelectOption,
        OAuthSelectPrompt,
    },
};
use crate::{
    auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError},
    types::{ModelCost, ModelInput, ThinkingLevelMap},
};

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
pub const CALLBACK_HOST: &str = "127.0.0.1";
pub const CALLBACK_PORT: u16 = 1456;
pub const CALLBACK_PATH: &str = "/oauth/callback";
pub const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";
pub const TOKEN_EXPIRY_SKEW_MS: i64 = 60_000;
pub const LOGIN_METHOD_BROWSER: &str = "browser";
pub const LOGIN_METHOD_DEVICE_CODE: &str = "device-code";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInput>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayConfig {
    pub base_url: String,
    pub models: Vec<RadiusGatewayModel>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusOAuthCredentials {
    #[serde(flatten)]
    pub credential: OAuthCredential,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_config: Option<RadiusGatewayConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusOAuthConfig {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub device_authorization_endpoint: String,
    #[serde(default)]
    pub device_authorization_events_endpoint: Option<String>,
    pub verification_endpoint: String,
    pub client_id: String,
    pub scope: String,
    pub device_code_grant_type: String,
}

pub fn normalize_radius_gateway_url(value: &str) -> String {
    let with_scheme = if value.starts_with("http://") || value.starts_with("https://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_owned()
}

pub fn sanitize_radius_gateway_config(config: &Value) -> Option<RadiusGatewayConfig> {
    let base_url = config.get("baseUrl")?.as_str()?.to_owned();
    let models = config.get("models")?.as_array()?;
    let models = models
        .iter()
        .filter_map(|model| {
            if !model.get("id")?.is_string()
                || !model.get("name")?.is_string()
                || !model.get("reasoning")?.is_boolean()
                || !model.get("input")?.is_array()
                || !model.get("cost")?.is_object()
                || !model.get("contextWindow")?.is_number()
                || !model.get("maxTokens")?.is_number()
            {
                return None;
            }
            serde_json::from_value(model.clone()).ok()
        })
        .collect();
    Some(RadiusGatewayConfig { base_url, models })
}

pub async fn load_radius_oauth_config(
    client: &reqwest::Client,
    gateway: &str,
) -> Result<RadiusOAuthConfig, OAuthError> {
    let url = format!("{gateway}/v1/oauth");
    let response = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::Other(format!(
            "Could not load Radius OAuth config from {gateway}: {status} {body}"
        )));
    }
    response
        .json()
        .await
        .map_err(|e| OAuthError::InvalidResponse(e.to_string()))
}

pub async fn load_radius_gateway_config(
    client: &reqwest::Client,
    gateway: &str,
    api_key: Option<&str>,
) -> Result<RadiusGatewayConfig, OAuthError> {
    let url = format!("{gateway}/v1/config");
    let mut req = client.get(&url).header("accept", "application/json");
    if let Some(key) = api_key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let response = req.send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let trimmed = body.trim();
        let body = if trimmed.len() > 512 {
            format!("{}…", &trimmed[..512])
        } else {
            trimmed.to_owned()
        };
        return Err(OAuthError::Other(format!(
            "Could not load Radius config from {gateway}: {status}: {body}"
        )));
    }
    let json: Value = response.json().await?;
    sanitize_radius_gateway_config(&json)
        .ok_or_else(|| OAuthError::Other(format!("Invalid Radius config from {gateway}")))
}

pub async fn request_oauth_token(
    client: &reqwest::Client,
    oauth: &RadiusOAuthConfig,
    body: &[(&str, &str)],
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(&oauth.token_endpoint)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .form(body)
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let (oauth_error, description) = parse_oauth_error_body(&text);
        let detail = match (oauth_error.as_deref(), description.as_deref()) {
            (Some(err), Some(desc)) => format!("{err}: {desc}"),
            (Some(err), None) => err.to_owned(),
            (None, Some(desc)) => desc.to_owned(),
            (None, None) => status.as_u16().to_string(),
        };
        return Err(OAuthError::Other(format!(
            "Radius OAuth token request failed: {detail}"
        )));
    }
    let data: Value = response.json().await?;
    let access = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::InvalidResponse("missing access_token".into()))?;
    let refresh = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::InvalidResponse("missing refresh_token".into()))?;
    let expires_in = data
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| OAuthError::InvalidResponse("missing expires_in".into()))?;
    let mut extra = std::collections::HashMap::new();
    if let Some(scope) = data.get("scope").and_then(Value::as_str) {
        extra.insert("scope".into(), Value::String(scope.into()));
    }
    Ok(OAuthCredential {
        access: access.into(),
        refresh: refresh.into(),
        expires: jiff::Timestamp::now().as_millisecond() + expires_in * 1000 - TOKEN_EXPIRY_SKEW_MS,
        extra,
    })
}

fn parse_oauth_error_body(text: &str) -> (Option<String>, Option<String>) {
    if text.is_empty() {
        return (None, None);
    }
    if let Ok(data) = serde_json::from_str::<Value>(text) {
        let err = data.get("error").and_then(Value::as_str).map(str::to_owned);
        let desc = data
            .get("error_description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        return (err, desc);
    }
    (None, Some(text.to_owned()))
}

pub struct RadiusOAuthProviderOptions {
    pub id: String,
    pub name: String,
    pub gateway: String,
}

pub struct RadiusOAuth {
    client: reqwest::Client,
    id: String,
    name: String,
    gateway: String,
    test_callback: Option<String>,
}

impl RadiusOAuth {
    pub fn create(options: RadiusOAuthProviderOptions) -> Self {
        Self::create_with_client(reqwest::Client::new(), options)
    }

    pub fn create_with_client(
        client: reqwest::Client,
        options: RadiusOAuthProviderOptions,
    ) -> Self {
        Self {
            client,
            id: options.id,
            name: options.name,
            gateway: normalize_radius_gateway_url(&options.gateway),
            test_callback: None,
        }
    }

    pub fn with_test_callback(mut self, code: impl Into<String>) -> Self {
        self.test_callback = Some(code.into());
        self
    }

    pub fn gateway(&self) -> &str {
        &self.gateway
    }

    async fn attach_gateway_config(
        &self,
        credentials: OAuthCredential,
        previous: Option<&OAuthCredential>,
    ) -> Result<OAuthCredential, OAuthError> {
        match load_radius_gateway_config(&self.client, &self.gateway, Some(&credentials.access))
            .await
        {
            Ok(config) => {
                let mut credentials = credentials;
                credentials.extra.insert(
                    "gatewayConfig".into(),
                    serde_json::to_value(config).unwrap_or(Value::Null),
                );
                Ok(credentials)
            }
            Err(error) => {
                if let Some(prev) = previous.and_then(|c| c.extra.get("gatewayConfig")).cloned() {
                    let mut credentials = credentials;
                    credentials.extra.insert("gatewayConfig".into(), prev);
                    Ok(credentials)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn login_browser(
        &self,
        oauth: &RadiusOAuthConfig,
        callbacks: &OAuthLoginCallbacks,
    ) -> Result<OAuthCredential, OAuthError> {
        let pkce = generate_pkce();
        let state = generate_state();
        let mut authorize = url::Url::parse(&oauth.authorization_endpoint)
            .map_err(|e| OAuthError::Other(format!("invalid authorization endpoint: {e}")))?;
        authorize
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &oauth.client_id)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", &oauth.scope)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("handoff", "url")
            .append_pair("state", &state);

        callbacks.progress(&format!("Listening for OAuth callback on {REDIRECT_URI}"));
        (callbacks.on_auth)(super::types::OAuthAuthInfo {
            url: authorize.to_string(),
            instructions: Some("Continue in your browser.".into()),
        });
        if callbacks.open_browser {
            browser::open_url(authorize.as_str());
        }

        let code = if let Some(code) = &self.test_callback {
            code.clone()
        } else {
            let server = start_callback_server(CallbackServerConfig {
                host: CALLBACK_HOST.into(),
                port: CALLBACK_PORT,
                path: CALLBACK_PATH.into(),
                expected_state: Some(state),
                success_message: "Signed in to Radius. You may now close this page.".into(),
            })
            .await?;
            let result = server.wait_for_code().await?;
            result.map(|r| r.code).ok_or_else(|| {
                if callbacks
                    .cancellation
                    .as_ref()
                    .is_some_and(|c| c.is_cancelled())
                {
                    OAuthError::Other("Login cancelled".into())
                } else {
                    OAuthError::Other("OAuth callback did not complete.".into())
                }
            })?
        };

        request_oauth_token(
            &self.client,
            oauth,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &oauth.client_id),
                ("redirect_uri", REDIRECT_URI),
                ("code", &code),
                ("code_verifier", &pkce.verifier),
            ],
        )
        .await
    }

    async fn login_device_code(
        &self,
        oauth: &RadiusOAuthConfig,
        callbacks: &OAuthLoginCallbacks,
    ) -> Result<OAuthCredential, OAuthError> {
        let response = self
            .client
            .post(&oauth.device_authorization_endpoint)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .form(&[
                ("client_id", oauth.client_id.as_str()),
                ("scope", oauth.scope.as_str()),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(OAuthError::Other(
                "Radius OAuth device authorization failed".into(),
            ));
        }
        let data: Value = response.json().await?;
        let device_code = data
            .get("device_code")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                OAuthError::Other(
                    "Radius OAuth device authorization response is missing required fields".into(),
                )
            })?;
        let user_code = data
            .get("user_code")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                OAuthError::Other(
                    "Radius OAuth device authorization response is missing required fields".into(),
                )
            })?;
        let expires_in = data
            .get("expires_in")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                OAuthError::Other(
                    "Radius OAuth device authorization response is missing required fields".into(),
                )
            })?;
        let interval = data.get("interval").and_then(Value::as_u64);
        let verification_uri = data
            .get("verification_uri")
            .and_then(Value::as_str)
            .unwrap_or(&oauth.verification_endpoint)
            .to_owned();

        (callbacks.on_device_code)(OAuthDeviceCodeInfo {
            user_code: user_code.into(),
            verification_uri,
            interval_seconds: interval,
            expires_in_seconds: Some(expires_in),
        });

        let client = self.client.clone();
        let oauth = oauth.clone();
        let device_code = device_code.to_owned();
        poll_oauth_device_code_flow(
            DeviceCodePollOptions {
                interval: interval.map(Duration::from_secs),
                expires_in: Some(Duration::from_secs(expires_in)),
                wait_before_first_poll: false,
                cancellation: callbacks.cancellation.clone(),
            },
            || {
                let client = client.clone();
                let oauth = oauth.clone();
                let device_code = device_code.clone();
                async move {
                    match request_oauth_token(
                        &client,
                        &oauth,
                        &[
                            ("grant_type", oauth.device_code_grant_type.as_str()),
                            ("client_id", oauth.client_id.as_str()),
                            ("device_code", device_code.as_str()),
                        ],
                    )
                    .await
                    {
                        Ok(credentials) => Ok(DeviceCodePoll::Complete(credentials)),
                        Err(OAuthError::Other(message))
                            if message.contains("authorization_pending") =>
                        {
                            Ok(DeviceCodePoll::Pending)
                        }
                        Err(OAuthError::Other(message)) if message.contains("slow_down") => {
                            Ok(DeviceCodePoll::SlowDown {
                                interval_seconds: None,
                            })
                        }
                        Err(OAuthError::Other(message)) if message.contains("expired_token") => Ok(
                            DeviceCodePoll::Failed("Device authorization expired.".into()),
                        ),
                        Err(OAuthError::Other(message)) if message.contains("access_denied") => Ok(
                            DeviceCodePoll::Failed("Device authorization was denied.".into()),
                        ),
                        Err(error) => Err(error),
                    }
                }
            },
        )
        .await
    }
}

#[async_trait]
impl OAuthAuth for RadiusOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn refresh(&self, credential: &OAuthCredential) -> Result<OAuthCredential, OAuthError> {
        let oauth = load_radius_oauth_config(&self.client, &self.gateway).await?;
        let refreshed = request_oauth_token(
            &self.client,
            &oauth,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", oauth.client_id.as_str()),
                ("refresh_token", credential.refresh.as_str()),
            ],
        )
        .await?;
        self.attach_gateway_config(refreshed, Some(credential))
            .await
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, OAuthError> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: credential
                .extra
                .get("gatewayConfig")
                .and_then(|cfg| cfg.get("baseUrl"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

#[async_trait]
impl OAuthProvider for RadiusOAuth {
    fn id(&self) -> &str {
        &self.id
    }

    fn uses_callback_server(&self) -> bool {
        true
    }

    async fn login(&self, callbacks: &OAuthLoginCallbacks) -> Result<OAuthCredential, OAuthError> {
        let oauth = load_radius_oauth_config(&self.client, &self.gateway).await?;
        let login_method = (callbacks.on_select)(OAuthSelectPrompt {
            message: format!("Sign in to {}:", self.name),
            options: vec![
                OAuthSelectOption {
                    id: LOGIN_METHOD_BROWSER.into(),
                    label: "Sign in with browser (recommended)".into(),
                },
                OAuthSelectOption {
                    id: LOGIN_METHOD_DEVICE_CODE.into(),
                    label: "Sign in with device code (when signing in from another device)".into(),
                },
            ],
        })
        .await
        .ok_or_else(|| OAuthError::Other("Login cancelled".into()))?;

        let credentials = if login_method == LOGIN_METHOD_DEVICE_CODE {
            self.login_device_code(&oauth, callbacks).await?
        } else if login_method == LOGIN_METHOD_BROWSER {
            self.login_browser(&oauth, callbacks).await?
        } else {
            return Err(OAuthError::Other(format!(
                "Unknown {} sign-in method: {login_method}",
                self.name
            )));
        };
        self.attach_gateway_config(credentials, None).await
    }
}

pub fn create_radius_oauth_provider(options: RadiusOAuthProviderOptions) -> RadiusOAuth {
    RadiusOAuth::create(options)
}
