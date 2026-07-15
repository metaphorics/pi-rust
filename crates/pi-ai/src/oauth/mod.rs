pub mod anthropic;
pub mod browser;
pub mod callback_server;
pub mod device_code;
pub mod github_copilot;
pub mod oauth_page;
pub mod openai_codex;
pub mod pkce;
pub mod radius;
pub mod types;

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

use parking_lot::RwLock;

use crate::auth::OAuthAuth;

pub use types::*;

static PROVIDERS: LazyLock<RwLock<HashMap<String, Arc<dyn OAuthAuth>>>> =
    LazyLock::new(|| RwLock::new(default_providers()));

static LOGIN_PROVIDERS: LazyLock<RwLock<HashMap<String, Arc<dyn OAuthProvider>>>> =
    LazyLock::new(|| RwLock::new(default_login_providers()));

fn default_providers() -> HashMap<String, Arc<dyn OAuthAuth>> {
    HashMap::from([
        (
            "anthropic".into(),
            Arc::new(anthropic::AnthropicOAuth::default()) as Arc<dyn OAuthAuth>,
        ),
        (
            "openai-codex".into(),
            Arc::new(openai_codex::OpenAICodexOAuth::default()) as Arc<dyn OAuthAuth>,
        ),
        (
            "github-copilot".into(),
            Arc::new(github_copilot::GitHubCopilotOAuth::default()) as Arc<dyn OAuthAuth>,
        ),
        (
            "radius".into(),
            Arc::new(radius::create_radius_oauth_provider(
                radius::RadiusOAuthProviderOptions {
                    id: "radius".into(),
                    name: "Radius".into(),
                    gateway: std::env::var("PI_GATEWAY")
                        .unwrap_or_else(|_| radius::DEFAULT_RADIUS_GATEWAY.into()),
                },
            )) as Arc<dyn OAuthAuth>,
        ),
    ])
}

fn default_login_providers() -> HashMap<String, Arc<dyn OAuthProvider>> {
    HashMap::from([
        (
            "anthropic".into(),
            Arc::new(anthropic::AnthropicOAuth::default()) as Arc<dyn OAuthProvider>,
        ),
        (
            "openai-codex".into(),
            Arc::new(openai_codex::OpenAICodexOAuth::default()) as Arc<dyn OAuthProvider>,
        ),
        (
            "github-copilot".into(),
            Arc::new(github_copilot::GitHubCopilotOAuth::default()) as Arc<dyn OAuthProvider>,
        ),
        (
            "radius".into(),
            Arc::new(radius::create_radius_oauth_provider(
                radius::RadiusOAuthProviderOptions {
                    id: "radius".into(),
                    name: "Radius".into(),
                    gateway: std::env::var("PI_GATEWAY")
                        .unwrap_or_else(|_| radius::DEFAULT_RADIUS_GATEWAY.into()),
                },
            )) as Arc<dyn OAuthProvider>,
        ),
    ])
}

pub fn get_oauth_provider(provider_id: &str) -> Option<Arc<dyn OAuthAuth>> {
    PROVIDERS.read().get(provider_id).cloned()
}

pub fn get_oauth_login_provider(provider_id: &str) -> Option<Arc<dyn OAuthProvider>> {
    LOGIN_PROVIDERS.read().get(provider_id).cloned()
}

pub fn register_oauth_provider(provider_id: impl Into<String>, provider: Arc<dyn OAuthAuth>) {
    PROVIDERS.write().insert(provider_id.into(), provider);
}

pub fn register_oauth_login_provider(
    provider_id: impl Into<String>,
    provider: Arc<dyn OAuthProvider>,
) {
    let id = provider_id.into();
    LOGIN_PROVIDERS
        .write()
        .insert(id.clone(), Arc::clone(&provider));
    PROVIDERS.write().insert(id, provider as Arc<dyn OAuthAuth>);
}

pub fn reset_oauth_providers() {
    *PROVIDERS.write() = default_providers();
    *LOGIN_PROVIDERS.write() = default_login_providers();
}
