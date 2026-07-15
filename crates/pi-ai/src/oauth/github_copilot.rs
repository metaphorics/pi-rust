use std::{sync::LazyLock, time::Duration};

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::{
    device_code::{DeviceCodePollOptions, poll_oauth_device_code_flow},
    types::{DeviceCodePoll, OAuthDeviceCodeInfo, OAuthLoginCallbacks, OAuthPrompt, OAuthProvider},
};
use crate::{
    auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError},
    models_generated::MODELS,
    types::Message,
};

pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const INDIVIDUAL_BASE_URL: &str = "https://api.individual.githubcopilot.com";
const COPILOT_API_VERSION: &str = "2026-06-01";

pub const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
pub const COPILOT_EDITOR_VERSION: &str = "vscode/1.107.0";
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

static PROXY_ENDPOINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"proxy-ep=([^;]+)").expect("Copilot proxy regex is valid"));

pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    url::Url::parse(&value).ok()?.host_str().map(str::to_owned)
}

pub fn get_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(host) = token
        .and_then(|token| PROXY_ENDPOINT.captures(token))
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str())
    {
        let api_host = host
            .strip_prefix("proxy.")
            .map_or_else(|| host.to_owned(), |suffix| format!("api.{suffix}"));
        return format!("https://{api_host}");
    }
    enterprise_domain
        .map(|domain| format!("https://copilot-api.{domain}"))
        .unwrap_or_else(|| INDIVIDUAL_BASE_URL.to_owned())
}

fn enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    credential
        .extra
        .get("enterpriseUrl")
        .and_then(Value::as_str)
        .and_then(normalize_domain)
}

fn copilot_urls(domain: &str) -> (String, String, String) {
    (
        format!("https://{domain}/login/device/code"),
        format!("https://{domain}/login/oauth/access_token"),
        format!("https://api.{domain}/copilot_internal/v2/token"),
    )
}

/// Dynamic request headers for Copilot API calls (port of github-copilot-headers.ts).
pub fn infer_copilot_initiator(messages: &[Message]) -> &'static str {
    match messages.last() {
        Some(Message::User(_)) => "user",
        Some(_) => "agent",
        None => "user",
    }
}

pub fn has_copilot_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|msg| {
        let parts: &[crate::types::Content] = match msg {
            Message::User(m) => match &m.content {
                crate::types::UserContent::Blocks(blocks) => blocks.as_slice(),
                crate::types::UserContent::Text(_) => return false,
            },
            Message::ToolResult(m) => m.content.as_slice(),
            Message::Assistant(_) => return false,
        };
        parts
            .iter()
            .any(|part| matches!(part, crate::types::Content::Image(_)))
    })
}

pub fn build_copilot_dynamic_headers(
    messages: &[Message],
    has_images: bool,
) -> std::collections::HashMap<String, String> {
    let mut headers = std::collections::HashMap::new();
    headers.insert(
        "X-Initiator".into(),
        infer_copilot_initiator(messages).into(),
    );
    headers.insert("Openai-Intent".into(), "conversation-edits".into());
    if has_images {
        headers.insert("Copilot-Vision-Request".into(), "true".into());
    }
    headers
}

#[derive(Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: i64,
}

#[derive(Clone, Debug)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Option<u64>,
    pub expires_in: u64,
}

pub async fn start_device_flow(
    client: &reqwest::Client,
    domain: &str,
    device_code_url: Option<&str>,
) -> Result<DeviceCodeResponse, OAuthError> {
    let (default_url, _, _) = copilot_urls(domain);
    let url = device_code_url.unwrap_or(&default_url);
    let raw: Value = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", COPILOT_USER_AGENT)
        .form(&[("client_id", CLIENT_ID), ("scope", "read:user")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let device_code = raw
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::InvalidResponse("Invalid device code response fields".into()))?;
    let user_code = raw
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::InvalidResponse("Invalid device code response fields".into()))?;
    let verification_uri = raw
        .get("verification_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::InvalidResponse("Invalid device code response fields".into()))?;
    let interval = raw.get("interval").and_then(Value::as_u64);
    let expires_in = raw
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| OAuthError::InvalidResponse("Invalid device code response fields".into()))?;

    let parsed = url::Url::parse(verification_uri).map_err(|_| {
        OAuthError::Other("Untrusted verification_uri in device code response".into())
    })?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(OAuthError::Other(
            "Untrusted verification_uri in device code response".into(),
        ));
    }

    Ok(DeviceCodeResponse {
        device_code: device_code.into(),
        user_code: user_code.into(),
        verification_uri: parsed.into(),
        interval,
        expires_in,
    })
}

pub async fn poll_for_github_access_token(
    client: &reqwest::Client,
    domain: &str,
    device: &DeviceCodeResponse,
    access_token_url: Option<&str>,
    cancellation: Option<super::device_code::CancellationFlag>,
) -> Result<String, OAuthError> {
    let (_, default_url, _) = copilot_urls(domain);
    let url = access_token_url.unwrap_or(&default_url).to_owned();
    let device_code = device.device_code.clone();
    let client = client.clone();
    poll_oauth_device_code_flow(
        DeviceCodePollOptions {
            interval: device.interval.map(Duration::from_secs),
            expires_in: Some(Duration::from_secs(device.expires_in)),
            wait_before_first_poll: true,
            cancellation,
        },
        || {
            let client = client.clone();
            let url = url.clone();
            let device_code = device_code.clone();
            async move {
                let response = client
                    .post(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .header("User-Agent", COPILOT_USER_AGENT)
                    .form(&[
                        ("client_id", CLIENT_ID),
                        ("device_code", device_code.as_str()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ])
                    .send()
                    .await?;
                let raw: Value = response.json().await?;
                if let Some(token) = raw.get("access_token").and_then(Value::as_str) {
                    return Ok(DeviceCodePoll::Complete(token.to_owned()));
                }
                if let Some(error) = raw.get("error").and_then(Value::as_str) {
                    if error == "authorization_pending" {
                        return Ok(DeviceCodePoll::Pending);
                    }
                    if error == "slow_down" {
                        return Ok(DeviceCodePoll::SlowDown {
                            interval_seconds: raw.get("interval").and_then(Value::as_u64),
                        });
                    }
                    let description = raw
                        .get("error_description")
                        .and_then(Value::as_str)
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default();
                    return Ok(DeviceCodePoll::Failed(format!(
                        "Device flow failed: {error}{description}"
                    )));
                }
                Ok(DeviceCodePoll::Failed(
                    "Invalid device token response".into(),
                ))
            }
        },
    )
    .await
}

pub async fn refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
    enterprise_domain: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    let domain = enterprise_domain.unwrap_or("github.com");
    let url = format!("https://api.{domain}/copilot_internal/v2/token");
    refresh_token_at(client, refresh_token, enterprise_domain, &url, None, true).await
}

async fn refresh_token_at(
    client: &reqwest::Client,
    refresh_token: &str,
    enterprise_domain: Option<&str>,
    url: &str,
    models_url_override: Option<&str>,
    fetch_models: bool,
) -> Result<OAuthCredential, OAuthError> {
    let token: CopilotTokenResponse = client
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {refresh_token}"))
        .header("User-Agent", COPILOT_USER_AGENT)
        .header("Editor-Version", COPILOT_EDITOR_VERSION)
        .header("Editor-Plugin-Version", COPILOT_EDITOR_PLUGIN_VERSION)
        .header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let mut extra = std::collections::HashMap::new();
    if fetch_models {
        let models_url = models_url_override.map(str::to_owned).unwrap_or_else(|| {
            format!(
                "{}/models",
                get_base_url(Some(&token.token), enterprise_domain)
            )
        });
        let models: Value = client
            .get(models_url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", token.token))
            .header("User-Agent", COPILOT_USER_AGENT)
            .header("Editor-Version", COPILOT_EDITOR_VERSION)
            .header("Editor-Plugin-Version", COPILOT_EDITOR_PLUGIN_VERSION)
            .header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
            .header("X-GitHub-Api-Version", COPILOT_API_VERSION)
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let available_model_ids = parse_available_model_ids(&models)?;
        extra.insert(
            "availableModelIds".into(),
            Value::Array(available_model_ids.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(domain) = enterprise_domain {
        extra.insert("enterpriseUrl".into(), Value::String(domain.into()));
    }
    Ok(OAuthCredential {
        access: token.token,
        refresh: refresh_token.to_owned(),
        expires: token.expires_at * 1000 - 300_000,
        extra,
    })
}

pub fn parse_available_model_ids(models: &Value) -> Result<Vec<String>, OAuthError> {
    let data = models
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| OAuthError::InvalidResponse("Invalid Copilot models response".into()))?;
    Ok(data
        .iter()
        .filter(|item| {
            item.get("model_picker_enabled").and_then(Value::as_bool) == Some(true)
                && item.pointer("/policy/state").and_then(Value::as_str) != Some("disabled")
                && item
                    .pointer("/capabilities/supports/tool_calls")
                    .and_then(Value::as_bool)
                    != Some(false)
        })
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

pub fn to_auth(credential: &OAuthCredential) -> ModelAuth {
    let domain = enterprise_domain(credential);
    ModelAuth {
        api_key: Some(credential.access.clone()),
        headers: None,
        base_url: Some(get_base_url(Some(&credential.access), domain.as_deref())),
    }
}

async fn enable_model(
    client: &reqwest::Client,
    token: &str,
    model_id: &str,
    enterprise_domain: Option<&str>,
) -> bool {
    let base = get_base_url(Some(token), enterprise_domain);
    let url = format!("{base}/models/{model_id}/policy");
    client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", COPILOT_USER_AGENT)
        .header("Editor-Version", COPILOT_EDITOR_VERSION)
        .header("Editor-Plugin-Version", COPILOT_EDITOR_PLUGIN_VERSION)
        .header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
        .header("openai-intent", "chat-policy")
        .header("x-interaction-type", "chat-policy")
        .json(&serde_json::json!({ "state": "enabled" }))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

async fn enable_all_models(client: &reqwest::Client, token: &str, enterprise_domain: Option<&str>) {
    let model_ids: Vec<&str> = MODELS
        .iter()
        .filter(|m| m.provider == "github-copilot")
        .map(|m| m.id)
        .collect();
    let futs = model_ids.into_iter().map(|id| {
        let client = client.clone();
        let token = token.to_owned();
        let domain = enterprise_domain.map(str::to_owned);
        async move {
            enable_model(&client, &token, id, domain.as_deref()).await;
        }
    });
    futures_util::future::join_all(futs).await;
}

pub struct GitHubCopilotOAuth {
    client: reqwest::Client,
    token_url_override: Option<String>,
    models_url_override: Option<String>,
    device_code_url_override: Option<String>,
    access_token_url_override: Option<String>,
    skip_enable_models: bool,
}

impl GitHubCopilotOAuth {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            token_url_override: None,
            models_url_override: None,
            device_code_url_override: None,
            access_token_url_override: None,
            skip_enable_models: false,
        }
    }

    pub fn with_endpoints(
        client: reqwest::Client,
        token_url: impl Into<String>,
        models_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            token_url_override: Some(token_url.into()),
            models_url_override: Some(models_url.into()),
            device_code_url_override: None,
            access_token_url_override: None,
            skip_enable_models: false,
        }
    }

    pub fn with_login_endpoints(
        client: reqwest::Client,
        device_code_url: impl Into<String>,
        access_token_url: impl Into<String>,
        token_url: impl Into<String>,
        models_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            token_url_override: Some(token_url.into()),
            models_url_override: Some(models_url.into()),
            device_code_url_override: Some(device_code_url.into()),
            access_token_url_override: Some(access_token_url.into()),
            skip_enable_models: true,
        }
    }
}

impl Default for GitHubCopilotOAuth {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

#[async_trait]
impl OAuthAuth for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    async fn refresh(&self, credential: &OAuthCredential) -> Result<OAuthCredential, OAuthError> {
        let domain = enterprise_domain(credential);
        if let Some(url) = &self.token_url_override {
            refresh_token_at(
                &self.client,
                &credential.refresh,
                domain.as_deref(),
                url,
                self.models_url_override.as_deref(),
                true,
            )
            .await
        } else {
            refresh_token(&self.client, &credential.refresh, domain.as_deref()).await
        }
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, OAuthError> {
        Ok(to_auth(credential))
    }
}

#[async_trait]
impl OAuthProvider for GitHubCopilotOAuth {
    fn id(&self) -> &str {
        "github-copilot"
    }

    async fn login(&self, callbacks: &OAuthLoginCallbacks) -> Result<OAuthCredential, OAuthError> {
        let input = (callbacks.on_prompt)(OAuthPrompt {
            message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
            placeholder: Some("company.ghe.com".into()),
            allow_empty: Some(true),
        })
        .await;

        if callbacks
            .cancellation
            .as_ref()
            .is_some_and(|c| c.is_cancelled())
        {
            return Err(OAuthError::Other("Login cancelled".into()));
        }

        let trimmed = input.trim();
        let enterprise = normalize_domain(trimmed);
        if !trimmed.is_empty() && enterprise.is_none() {
            return Err(OAuthError::Other(
                "Invalid GitHub Enterprise URL/domain".into(),
            ));
        }
        let domain = enterprise.clone().unwrap_or_else(|| "github.com".into());

        let device = start_device_flow(
            &self.client,
            &domain,
            self.device_code_url_override.as_deref(),
        )
        .await?;
        (callbacks.on_device_code)(OAuthDeviceCodeInfo {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
        });

        let github_access = poll_for_github_access_token(
            &self.client,
            &domain,
            &device,
            self.access_token_url_override.as_deref(),
            callbacks.cancellation.clone(),
        )
        .await?;

        let token_url = self.token_url_override.clone().unwrap_or_else(|| {
            let (_, _, u) = copilot_urls(&domain);
            u
        });
        let mut credentials = refresh_token_at(
            &self.client,
            &github_access,
            enterprise.as_deref(),
            &token_url,
            self.models_url_override.as_deref(),
            false,
        )
        .await?;

        if !self.skip_enable_models {
            callbacks.progress("Enabling models...");
            enable_all_models(&self.client, &credentials.access, enterprise.as_deref()).await;
        }

        // Fetch availability after policy enable.
        let models_url = self.models_url_override.clone().unwrap_or_else(|| {
            format!(
                "{}/models",
                get_base_url(Some(&credentials.access), enterprise.as_deref())
            )
        });
        let models: Value = self
            .client
            .get(models_url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", credentials.access))
            .header("User-Agent", COPILOT_USER_AGENT)
            .header("Editor-Version", COPILOT_EDITOR_VERSION)
            .header("Editor-Plugin-Version", COPILOT_EDITOR_PLUGIN_VERSION)
            .header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
            .header("X-GitHub-Api-Version", COPILOT_API_VERSION)
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let available = parse_available_model_ids(&models)?;
        credentials.extra.insert(
            "availableModelIds".into(),
            Value::Array(available.into_iter().map(Value::String).collect()),
        );
        if let Some(domain) = enterprise {
            credentials
                .extra
                .insert("enterpriseUrl".into(), Value::String(domain));
        }
        Ok(credentials)
    }
}
