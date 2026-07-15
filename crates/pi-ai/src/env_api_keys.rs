use std::{collections::HashMap, path::PathBuf};

use crate::types::ProviderEnv;

pub fn get_api_key_env_vars(provider: &str) -> Option<&'static [&'static str]> {
    Some(match provider {
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "anthropic" => &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "radius" => &["PI_GATEWAY_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        _ => return None,
    })
}

fn env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    env.and_then(|values| values.get(name).cloned())
        .or_else(|| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
}

pub fn find_env_keys(provider: &str, env: Option<&ProviderEnv>) -> Option<Vec<&'static str>> {
    let found: Vec<_> = get_api_key_env_vars(provider)?
        .iter()
        .copied()
        .filter(|name| env_value(name, env).is_some())
        .collect();
    (!found.is_empty()).then_some(found)
}

fn has_vertex_adc_credentials(env: Option<&ProviderEnv>) -> bool {
    if let Some(path) = env_value("GOOGLE_APPLICATION_CREDENTIALS", env) {
        return std::path::Path::new(&path).exists();
    }
    let Some(home) = env_value("HOME", env) else {
        return false;
    };
    PathBuf::from(home)
        .join(".config/gcloud/application_default_credentials.json")
        .exists()
}

pub fn get_env_api_key(provider: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(name) = find_env_keys(provider, env).and_then(|names| names.first().copied()) {
        return env_value(name, env);
    }

    if provider == "google-vertex"
        && has_vertex_adc_credentials(env)
        && (env_value("GOOGLE_CLOUD_PROJECT", env).is_some()
            || env_value("GCLOUD_PROJECT", env).is_some())
        && env_value("GOOGLE_CLOUD_LOCATION", env).is_some()
    {
        return Some("<authenticated>".into());
    }

    if provider == "amazon-bedrock"
        && (env_value("AWS_PROFILE", env).is_some()
            || (env_value("AWS_ACCESS_KEY_ID", env).is_some()
                && env_value("AWS_SECRET_ACCESS_KEY", env).is_some())
            || env_value("AWS_BEARER_TOKEN_BEDROCK", env).is_some()
            || env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env).is_some()
            || env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env).is_some()
            || env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env).is_some())
    {
        return Some("<authenticated>".into());
    }

    None
}

pub fn env_map(values: impl IntoIterator<Item = (String, String)>) -> ProviderEnv {
    values.into_iter().collect::<HashMap<_, _>>()
}
