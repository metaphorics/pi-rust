pub mod anthropic;
pub mod device_code;
pub mod github_copilot;
pub mod openai_codex;
pub mod pkce;
pub mod radius;
pub mod types;

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, RwLock},
};

use crate::auth::OAuthAuth;

pub use types::*;

static PROVIDERS: LazyLock<RwLock<HashMap<String, Arc<dyn OAuthAuth>>>> =
    LazyLock::new(|| RwLock::new(default_providers()));

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
    ])
}

pub fn get_oauth_provider(provider_id: &str) -> Option<Arc<dyn OAuthAuth>> {
    PROVIDERS
        .read()
        .expect("OAuth registry lock poisoned")
        .get(provider_id)
        .cloned()
}

pub fn register_oauth_provider(provider_id: impl Into<String>, provider: Arc<dyn OAuthAuth>) {
    PROVIDERS
        .write()
        .expect("OAuth registry lock poisoned")
        .insert(provider_id.into(), provider);
}

pub fn reset_oauth_providers() {
    *PROVIDERS.write().expect("OAuth registry lock poisoned") = default_providers();
}
