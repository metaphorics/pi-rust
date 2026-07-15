use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{ModelAuth, OAuthAuth, OAuthCredential, OAuthError};

pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const INDIVIDUAL_BASE_URL: &str = "https://api.individual.githubcopilot.com";
const COPILOT_API_VERSION: &str = "2026-06-01";

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

#[derive(Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: i64,
}

pub async fn refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
    enterprise_domain: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    let domain = enterprise_domain.unwrap_or("github.com");
    let url = format!("https://api.{domain}/copilot_internal/v2/token");
    refresh_token_at(client, refresh_token, enterprise_domain, &url, None).await
}

async fn refresh_token_at(
    client: &reqwest::Client,
    refresh_token: &str,
    enterprise_domain: Option<&str>,
    url: &str,
    models_url_override: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    let token: CopilotTokenResponse = client
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {refresh_token}"))
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .header("Editor-Version", "vscode/1.107.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

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
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .header("Editor-Version", "vscode/1.107.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("X-GitHub-Api-Version", COPILOT_API_VERSION)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let available_model_ids = parse_available_model_ids(&models)?;

    let mut extra = std::collections::HashMap::new();
    extra.insert(
        "availableModelIds".into(),
        Value::Array(available_model_ids.into_iter().map(Value::String).collect()),
    );
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

/// The injected client makes refresh deterministic to exercise against a local
/// HTTP fixture without changing production token logic.
pub struct GitHubCopilotOAuth {
    client: reqwest::Client,
    token_url_override: Option<String>,
    models_url_override: Option<String>,
}

impl GitHubCopilotOAuth {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            token_url_override: None,
            models_url_override: None,
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
