use std::collections::HashMap;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth_storage::{AuthStorage, AuthStatus};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverrideCost {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<pi_ai::types::ModelCostTier>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<pi_ai::types::ModelCostTier>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<pi_ai::types::ThinkingLevelMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<pi_ai::types::ModelInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<DefinitionCost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProviderType {
    Radius,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<pi_ai::types::ThinkingLevelMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<pi_ai::types::ModelInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<OverrideCost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthProviderType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<HashMap<String, ModelOverride>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Clone, Debug)]
struct ProviderOverride {
    base_url: Option<String>,
    compat: Option<Value>,
}

#[derive(Clone, Debug)]
struct ProviderRequestConfig {
    api_key: Option<String>,
    headers: Option<HashMap<String, String>>,
    auth_header: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRequestAuth {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfigInput {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub auth_header: Option<bool>,
    pub oauth: Option<Value>,
    pub models: Option<Vec<ModelDefinition>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolveResult {
    pub model: Option<pi_ai::types::Model>,
    pub thinking_level: Option<pi_ai::types::ModelThinkingLevel>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

type LoadedCustomModels = (
    Vec<pi_ai::types::Model>,
    HashMap<String, ProviderOverride>,
    HashMap<String, HashMap<String, ModelOverride>>,
);

pub struct ModelRegistry {
    models: Vec<pi_ai::types::Model>,
    provider_request_configs: HashMap<String, ProviderRequestConfig>,
    model_request_headers: HashMap<String, HashMap<String, String>>,
    config_model_overrides: HashMap<String, HashMap<String, ModelOverride>>,
    registered_providers: HashMap<String, ProviderConfigInput>,
    load_error: Option<String>,
    pub auth_storage: AuthStorage,
    models_json_path: Option<PathBuf>,
}

impl ModelRegistry {
    pub fn create(auth_storage: AuthStorage, models_json_path: PathBuf) -> Self {
        let mut registry = Self {
            models: Vec::new(),
            provider_request_configs: HashMap::new(),
            model_request_headers: HashMap::new(),
            config_model_overrides: HashMap::new(),
            registered_providers: HashMap::new(),
            load_error: None,
            auth_storage,
            models_json_path: Some(models_json_path),
        };
        registry.load_models();
        registry
    }

    pub fn in_memory(auth_storage: AuthStorage) -> Self {
        let mut registry = Self {
            models: Vec::new(),
            provider_request_configs: HashMap::new(),
            model_request_headers: HashMap::new(),
            config_model_overrides: HashMap::new(),
            registered_providers: HashMap::new(),
            load_error: None,
            auth_storage,
            models_json_path: None,
        };
        registry.load_models();
        registry
    }

    pub fn refresh(&mut self) {
        self.provider_request_configs.clear();
        self.model_request_headers.clear();
        self.load_error = None;

        self.load_models();

        let reg = self.registered_providers.clone();
        for (provider_name, config) in &reg {
            self.apply_provider_config(provider_name, config);
        }
    }

    pub fn get_error(&self) -> Option<String> {
        self.load_error.clone()
    }

    pub fn get_all(&self) -> &[pi_ai::types::Model] {
        &self.models
    }

    pub async fn get_available(&self) -> Vec<pi_ai::types::Model> {
        let mut available = Vec::new();
        for m in &self.models {
            if self.has_configured_auth(m).await {
                available.push(m.clone());
            }
        }
        available
    }

    pub fn find(&self, provider: &str, model_id: &str) -> Option<&pi_ai::types::Model> {
        self.models.iter().find(|m| m.provider == provider && m.id == model_id)
    }

    pub async fn has_configured_auth(&self, model: &pi_ai::types::Model) -> bool {
        let provider_api_key = self.provider_request_configs.get(&model.provider).and_then(|pc| pc.api_key.as_ref());
        if self.auth_storage.has_auth(&model.provider).await.unwrap_or(false) {
            return true;
        }
        if let Some(key) = provider_api_key {
            let provider_env = self.auth_storage.get_provider_env(&model.provider).await.ok().flatten();
            crate::resolve_config_value::is_config_value_configured(key, provider_env.as_ref())
        } else {
            false
        }
    }

    pub async fn get_api_key_and_headers(&self, model: &pi_ai::types::Model) -> ResolvedRequestAuth {
        let provider_config = self.provider_request_configs.get(&model.provider);
        let provider_env = match self.auth_storage.get_provider_env(&model.provider).await {
            Ok(env) => env,
            Err(e) => return ResolvedRequestAuth {
                ok: false,
                error: Some(e.to_string()),
                api_key: None,
                headers: None,
                env: None,
            },
        };

        let api_key_from_auth_storage = match self.auth_storage.get_api_key(&model.provider, false).await {
            Ok(key) => key,
            Err(e) => return ResolvedRequestAuth {
                ok: false,
                error: Some(e.to_string()),
                api_key: None,
                headers: None,
                env: None,
            },
        };

        let api_key = match api_key_from_auth_storage {
            Some(key) => Some(key),
            None => {
                if let Some(pc) = provider_config {
                    if let Some(pc_api_key) = &pc.api_key {
                        match crate::resolve_config_value::resolve_config_value_or_throw(
                            pc_api_key,
                            &format!("API key for provider \"{}\"", model.provider),
                            provider_env.as_ref(),
                        ) {
                            Ok(val) => Some(val),
                            Err(e) => return ResolvedRequestAuth {
                                ok: false,
                                error: Some(e),
                                api_key: None,
                                headers: None,
                                env: None,
                            },
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        let provider_headers = if let Some(pc) = provider_config {
            match crate::resolve_config_value::resolve_headers_or_throw(
                pc.headers.as_ref(),
                &format!("provider \"{}\"", model.provider),
                provider_env.as_ref(),
            ) {
                Ok(h) => h,
                Err(e) => return ResolvedRequestAuth {
                    ok: false,
                    error: Some(e),
                    api_key: None,
                    headers: None,
                    env: None,
                },
            }
        } else {
            None
        };

        let model_request_key = format!("{}:{}", model.provider, model.id);
        let model_headers_config = self.model_request_headers.get(&model_request_key);
        let model_headers = match crate::resolve_config_value::resolve_headers_or_throw(
            model_headers_config,
            &format!("model \"{}/{}\"", model.provider, model.id),
            provider_env.as_ref(),
        ) {
            Ok(h) => h,
            Err(e) => return ResolvedRequestAuth {
                ok: false,
                error: Some(e),
                api_key: None,
                headers: None,
                env: None,
            },
        };

        let mut headers = HashMap::new();
        if let Some(mh) = &model.headers {
            for (k, v) in mh {
                headers.insert(k.clone(), v.clone());
            }
        }
        if let Some(ph) = &provider_headers {
            for (k, v) in ph {
                headers.insert(k.clone(), v.clone());
            }
        }
        if let Some(mh) = &model_headers {
            for (k, v) in mh {
                headers.insert(k.clone(), v.clone());
            }
        }

        if provider_config.and_then(|pc| pc.auth_header).unwrap_or(false) {
            let Some(key) = &api_key else {
                return ResolvedRequestAuth {
                    ok: false,
                    error: Some(format!("No API key found for \"{}\"", model.provider)),
                    api_key: None,
                    headers: None,
                    env: None,
                };
            };
            headers.insert("Authorization".to_string(), format!("Bearer {}", key));
        }

        let final_headers = if headers.is_empty() { None } else { Some(headers) };
        let final_env = if let Some(e) = &provider_env {
            if e.is_empty() { None } else { Some(e.clone()) }
        } else {
            None
        };

        ResolvedRequestAuth {
            ok: true,
            error: None,
            api_key,
            headers: final_headers,
            env: final_env,
        }
    }

    pub async fn get_provider_auth_status(&self, provider: &str) -> AuthStatus {
        let auth_status = match self.auth_storage.get_auth_status(provider).await {
            Ok(status) => status,
            Err(_) => AuthStatus {
                configured: false,
                source: None,
                label: None,
            },
        };

        if auth_status.source.is_some() {
            return auth_status;
        }

        let provider_api_key = self.provider_request_configs.get(provider).and_then(|pc| pc.api_key.as_ref());
        let Some(key) = provider_api_key else {
            return auth_status;
        };

        if crate::resolve_config_value::is_command_config_value(key) {
            return AuthStatus {
                configured: true,
                source: Some("models_json_command".to_string()),
                label: None,
            };
        }

        let env_var_names = crate::resolve_config_value::get_config_value_env_var_names(key);
        if !env_var_names.is_empty() {
            let provider_env = self.auth_storage.get_provider_env(provider).await.ok().flatten();
            return if crate::resolve_config_value::is_config_value_configured(key, provider_env.as_ref()) {
                AuthStatus {
                    configured: true,
                    source: Some("environment".to_string()),
                    label: Some(env_var_names.join(", ")),
                }
            } else {
                AuthStatus {
                    configured: false,
                    source: None,
                    label: None,
                }
            };
        }

        AuthStatus {
            configured: true,
            source: Some("models_json_key".to_string()),
            label: None,
        }
    }

    pub fn get_provider_display_name(&self, provider: &str) -> String {
        if let Some(name) = self.registered_providers.get(provider).and_then(|r| r.name.as_ref()) {
            return name.clone();
        }

        match provider {
            "openai" => "OpenAI".to_string(),
            "anthropic" => "Anthropic".to_string(),
            "google" => "Google AI".to_string(),
            "google-vertex" => "Vertex AI".to_string(),
            "amazon-bedrock" => "Amazon Bedrock".to_string(),
            "mistral" => "Mistral".to_string(),
            "openrouter" => "OpenRouter".to_string(),
            "ollama" => "Ollama".to_string(),
            "lm-studio" => "LM Studio".to_string(),
            "groq" => "Groq".to_string(),
            other => other.to_string(),
        }
    }

    pub async fn get_api_key_for_provider(&self, provider: &str) -> Option<String> {
        if let Ok(Some(key)) = self.auth_storage.get_api_key(provider, true).await {
            return Some(key);
        }

        let provider_api_key = self.provider_request_configs.get(provider).and_then(|pc| pc.api_key.as_ref());
        if let Some(key) = provider_api_key {
            let provider_env = self.auth_storage.get_provider_env(provider).await.ok().flatten();
            crate::resolve_config_value::resolve_config_value_uncached(key, provider_env.as_ref())
        } else {
            None
        }
    }

    pub async fn is_using_oauth(&self, model: &pi_ai::types::Model) -> bool {
        matches!(self.auth_storage.get(&model.provider).await, Ok(Some(pi_ai::auth::Credential::OAuth(_))))
    }

    pub fn register_provider(&mut self, provider_name: String, config: ProviderConfigInput) -> Result<(), String> {
        self.validate_provider_config(&provider_name, &config)?;
        self.apply_provider_config(&provider_name, &config);
        self.upsert_registered_provider(provider_name, config);
        Ok(())
    }

    pub fn unregister_provider(&mut self, provider_name: &str) {
        if !self.registered_providers.contains_key(provider_name) {
            return;
        }
        self.registered_providers.remove(provider_name);
        self.refresh();
    }

    fn validate_provider_config(&self, provider_name: &str, config: &ProviderConfigInput) -> Result<(), String> {
        let models = config.models.as_deref().unwrap_or(&[]);
        if models.is_empty() {
            return Ok(());
        }

        if config.base_url.is_none() {
            return Err(format!("Provider {}: \"baseUrl\" is required when defining models.", provider_name));
        }

        if config.api_key.is_none() && config.oauth.is_none() {
            return Err(format!("Provider {}: \"apiKey\" or \"oauth\" is required when defining models.", provider_name));
        }

        for model_def in models {
            let api = model_def.api.as_ref().or(config.api.as_ref());
            if api.is_none() {
                return Err(format!("Provider {}, model {}: no \"api\" specified.", provider_name, model_def.id));
            }
        }
        Ok(())
    }

    fn apply_provider_config(&mut self, provider_name: &str, config: &ProviderConfigInput) {
        let req_cfg = ProviderRequestConfig {
            api_key: config.api_key.clone(),
            headers: config.headers.clone(),
            auth_header: config.auth_header,
        };
        self.provider_request_configs.insert(provider_name.to_string(), req_cfg);

        let models = config.models.as_deref().unwrap_or(&[]);
        if !models.is_empty() {
            self.models.retain(|m| m.provider != provider_name);

            for model_def in models {
                let api = model_def.api.as_ref().or(config.api.as_ref()).cloned().unwrap_or_default();
                let model_override = self.get_configured_model_override(provider_name, &model_def.id);

                let mut headers = HashMap::new();
                if let Some(mh) = &model_def.headers {
                    for (k, v) in mh {
                        headers.insert(k.clone(), v.clone());
                    }
                }
                if let Some(mo_h) = model_override.as_ref().and_then(|mo| mo.headers.as_ref()) {
                    for (k, v) in mo_h {
                        headers.insert(k.clone(), v.clone());
                    }
                }
                let headers_opt = if headers.is_empty() { None } else { Some(headers) };
                if let Some(h) = &headers_opt {
                    self.model_request_headers.insert(format!("{}:{}", provider_name, model_def.id), h.clone());
                }

                let base_url = model_def.base_url.as_ref().or(config.base_url.as_ref()).cloned().unwrap_or_default();

                let input = model_def.input.clone().unwrap_or_else(|| vec![pi_ai::types::ModelInput::Text]);
                let cost = model_def.cost.as_ref().map(|c| pi_ai::types::ModelCost {
                    input: c.input,
                    output: c.output,
                    cache_read: c.cache_read,
                    cache_write: c.cache_write,
                    tiers: c.tiers.clone().unwrap_or_default(),
                }).unwrap_or_default();
                let context_window = model_def.context_window.unwrap_or(128000);
                let max_tokens = model_def.max_tokens.unwrap_or(16384);

                let mut m = pi_ai::types::Model {
                    id: model_def.id.clone(),
                    name: model_def.name.clone().unwrap_or_else(|| model_def.id.clone()),
                    api: pi_ai::types::Api(api),
                    provider: provider_name.to_string(),
                    base_url,
                    reasoning: model_def.reasoning.unwrap_or(false),
                    thinking_level_map: model_def.thinking_level_map.clone(),
                    input,
                    cost,
                    context_window,
                    max_tokens,
                    headers: None,
                    compat: model_def.compat.clone(),
                };

                if let Some(mo) = &model_override {
                    m = apply_model_override(m, mo);
                }

                self.models.push(m);
            }
        } else if let Some(bu) = &config.base_url {
            for m in &mut self.models {
                if m.provider == provider_name {
                    m.base_url = bu.clone();
                }
            }
        }
    }

    fn upsert_registered_provider(&mut self, provider_name: String, config: ProviderConfigInput) {
        if let Some(existing) = self.registered_providers.get_mut(&provider_name) {
            if config.name.is_some() {
                existing.name = config.name.clone();
            }
            if config.base_url.is_some() {
                existing.base_url = config.base_url.clone();
            }
            if config.api_key.is_some() {
                existing.api_key = config.api_key.clone();
            }
            if config.api.is_some() {
                existing.api = config.api.clone();
            }
            if config.headers.is_some() {
                existing.headers = config.headers.clone();
            }
            if config.auth_header.is_some() {
                existing.auth_header = config.auth_header;
            }
            if config.oauth.is_some() {
                existing.oauth = config.oauth.clone();
            }
            if config.models.is_some() {
                existing.models = config.models.clone();
            }
        } else {
            self.registered_providers.insert(provider_name, config);
        }
    }

    fn load_models(&mut self) {
        let (custom_models, overrides, model_overrides) = if let Some(path) = self.models_json_path.clone() {
            match self.load_custom_models(&path) {
                Ok(res) => res,
                Err(err_msg) => {
                    self.load_error = Some(err_msg);
                    (Vec::new(), HashMap::new(), HashMap::new())
                }
            }
        } else {
            (Vec::new(), HashMap::new(), HashMap::new())
        };

        self.config_model_overrides = model_overrides.clone();

        let built_in = self.load_built_in_models(&overrides, &model_overrides);
        self.models = self.merge_custom_models(built_in, custom_models);
    }

    fn load_custom_models(&mut self, path: &std::path::Path) -> Result<LoadedCustomModels, String> {
        if !path.exists() {
            return Ok((Vec::new(), HashMap::new(), HashMap::new()));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read models.json: {}\n\nFile: {}", e, path.display()))?;
        let stripped = strip_json_comments(&content);
        let config: ModelsConfig = serde_json::from_str(&stripped)
            .map_err(|e| format!("Failed to parse models.json: {}\n\nFile: {}", e, path.display()))?;

        validate_config(&config)?;

        let mut overrides = HashMap::new();
        let mut model_overrides = HashMap::new();

        for (provider_name, provider_config) in &config.providers {
            if provider_config.base_url.is_some() || provider_config.compat.is_some() {
                overrides.insert(
                    provider_name.clone(),
                    ProviderOverride {
                        base_url: provider_config.base_url.clone(),
                        compat: provider_config.compat.clone(),
                    },
                );
            }

            let req_cfg = ProviderRequestConfig {
                api_key: provider_config.api_key.clone(),
                headers: provider_config.headers.clone(),
                auth_header: provider_config.auth_header,
            };
            self.provider_request_configs.insert(provider_name.clone(), req_cfg);

            if let Some(mo_map) = &provider_config.model_overrides {
                model_overrides.insert(provider_name.clone(), mo_map.clone());
                for (model_id, mo) in mo_map {
                    if let Some(h) = &mo.headers {
                        self.model_request_headers.insert(format!("{}:{}", provider_name, model_id), h.clone());
                    }
                }
            }
        }

        let custom_models = self.parse_models(&config);
        Ok((custom_models, overrides, model_overrides))
    }

    fn parse_models(&mut self, config: &ModelsConfig) -> Vec<pi_ai::types::Model> {
        let mut models = Vec::new();

        for (provider_name, provider_config) in &config.providers {
            let model_defs = match &provider_config.models {
                Some(defs) => defs.as_slice(),
                None => continue,
            };

            let built_in_defaults = get_built_in_defaults(provider_name);

            for model_def in model_defs {
                let api = model_def
                    .api
                    .as_ref()
                    .or(provider_config.api.as_ref())
                    .or(built_in_defaults.as_ref().map(|d| &d.0))
                    .cloned();

                let Some(api_str) = api else {
                    continue;
                };

                let base_url = model_def
                    .base_url
                    .as_ref()
                    .or(provider_config.base_url.as_ref())
                    .or(built_in_defaults.as_ref().map(|d| &d.1))
                    .cloned();

                let Some(base_url_str) = base_url else {
                    continue;
                };

                let compat = merge_json_values(provider_config.compat.clone(), model_def.compat.clone());
                if let Some(h) = &model_def.headers {
                    self.model_request_headers.insert(format!("{}:{}", provider_name, model_def.id), h.clone());
                }

                let input = model_def.input.clone().unwrap_or_else(|| vec![pi_ai::types::ModelInput::Text]);
                let cost = model_def.cost.as_ref().map(|c| pi_ai::types::ModelCost {
                    input: c.input,
                    output: c.output,
                    cache_read: c.cache_read,
                    cache_write: c.cache_write,
                    tiers: c.tiers.clone().unwrap_or_default(),
                }).unwrap_or_default();
                let context_window = model_def.context_window.unwrap_or(128000);
                let max_tokens = model_def.max_tokens.unwrap_or(16384);

                models.push(pi_ai::types::Model {
                    id: model_def.id.clone(),
                    name: model_def.name.clone().unwrap_or_else(|| model_def.id.clone()),
                    api: pi_ai::types::Api(api_str),
                    provider: provider_name.clone(),
                    base_url: base_url_str,
                    reasoning: model_def.reasoning.unwrap_or(false),
                    thinking_level_map: model_def.thinking_level_map.clone(),
                    input,
                    cost,
                    context_window,
                    max_tokens,
                    headers: None,
                    compat,
                });
            }
        }
        models
    }

    fn load_built_in_models(
        &self,
        overrides: &HashMap<String, ProviderOverride>,
        model_overrides: &HashMap<String, HashMap<String, ModelOverride>>,
    ) -> Vec<pi_ai::types::Model> {
        let mut built_in = Vec::new();
        for entry in pi_ai::models_generated::MODELS {
            let mut model = match entry.to_model() {
                Ok(m) => m,
                _ => continue,
            };

            if let Some(po) = overrides.get(&model.provider) {
                if let Some(bu) = &po.base_url {
                    model.base_url = bu.clone();
                }
                model.compat = merge_json_values(model.compat, po.compat.clone());
            }

            if let Some(mo) = model_overrides.get(&model.provider).and_then(|mo_map| mo_map.get(&model.id)) {
                model = apply_model_override(model, mo);
            }

            built_in.push(model);
        }
        built_in
    }

    fn merge_custom_models(
        &self,
        built_in: Vec<pi_ai::types::Model>,
        custom: Vec<pi_ai::types::Model>,
    ) -> Vec<pi_ai::types::Model> {
        let mut merged = built_in;
        for custom_model in custom {
            if let Some(idx) = merged
                .iter()
                .position(|m| m.provider == custom_model.provider && m.id == custom_model.id)
            {
                merged[idx] = custom_model;
            } else {
                merged.push(custom_model);
            }
        }
        merged
    }

    fn get_configured_model_override(&self, provider_name: &str, model_id: &str) -> Option<ModelOverride> {
        self.config_model_overrides.get(provider_name).and_then(|mo| mo.get(model_id).cloned())
    }

    pub async fn resolve(
        &self,
        cli_provider: Option<&str>,
        cli_model: Option<&str>,
        cli_thinking: Option<pi_ai::types::ModelThinkingLevel>,
    ) -> ResolveResult {
        let Some(cli_model_str) = cli_model else {
            return ResolveResult {
                model: None,
                thinking_level: None,
                warning: None,
                error: None,
            };
        };

        let available_models = self.models.clone();
        if available_models.is_empty() {
            return ResolveResult {
                model: None,
                thinking_level: None,
                warning: None,
                error: Some("No models available. Check your installation or add models to models.json.".to_string()),
            };
        }

        // Build canonical provider lookup (case-insensitive)
        let mut provider_map = HashMap::new();
        for m in &available_models {
            provider_map.insert(m.provider.to_lowercase(), m.provider.clone());
        }

        let mut provider = cli_provider.and_then(|p| provider_map.get(&p.to_lowercase()).cloned());
        if cli_provider.is_some() && provider.is_none() {
            return ResolveResult {
                model: None,
                thinking_level: None,
                warning: None,
                error: Some(format!(
                    "Unknown provider \"{}\". Use --list-models to see available providers/models.",
                    cli_provider.unwrap_or_default()
                )),
            };
        }

        let mut pattern = cli_model_str.to_string();
        let mut inferred_provider = false;

        if provider.is_none() && cli_model_str.contains('/') {
            let slash_idx = cli_model_str.find('/').unwrap();
            let maybe_provider = &cli_model_str[..slash_idx];
            if let Some(canonical) = provider_map.get(&maybe_provider.to_lowercase()) {
                provider = Some(canonical.clone());
                pattern = cli_model_str[slash_idx + 1..].to_string();
                inferred_provider = true;
            }
        }

        if provider.is_none() {
            let lower = cli_model_str.to_lowercase();
            let exact = available_models.iter().find(|m| {
                m.id.to_lowercase() == lower || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
            });
            if let Some(m) = exact {
                return ResolveResult {
                    model: Some(m.clone()),
                    thinking_level: None,
                    warning: None,
                    error: None,
                };
            }
        }

        if let (Some(p), true) = (&provider, cli_provider.is_some()) {
            let prefix = format!("{}/", p);
            if cli_model_str.to_lowercase().starts_with(&prefix.to_lowercase()) {
                pattern = cli_model_str[prefix.len()..].to_string();
            }
        }

        let candidates = if let Some(p) = &provider {
            available_models.iter().filter(|m| &m.provider == p).cloned().collect()
        } else {
            available_models.clone()
        };

        let (model, thinking_level, warning) = parse_model_pattern(&pattern, &candidates, false);

        if let Some(m) = &model {
            if inferred_provider {
                let raw_exact_matches: Vec<_> = available_models
                    .iter()
                    .filter(|x| x.id.to_lowercase() == cli_model_str.to_lowercase() && (x.id != m.id || x.provider != m.provider))
                    .collect();
                if !raw_exact_matches.is_empty() && !self.has_configured_auth(m).await {
                    let mut authenticated_raw_matches = Vec::new();
                    for x in raw_exact_matches {
                        if self.has_configured_auth(x).await {
                            authenticated_raw_matches.push(x.clone());
                        }
                    }
                    if authenticated_raw_matches.len() == 1 {
                        return ResolveResult {
                            model: Some(authenticated_raw_matches[0].clone()),
                            thinking_level: None,
                            warning: None,
                            error: None,
                        };
                    }
                }
            }
            return ResolveResult {
                model: Some(m.clone()),
                thinking_level,
                warning,
                error: None,
            };
        }

        if inferred_provider {
            let lower = cli_model_str.to_lowercase();
            let exact = available_models.iter().find(|m| {
                m.id.to_lowercase() == lower || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
            });
            if let Some(m) = exact {
                return ResolveResult {
                    model: Some(m.clone()),
                    thinking_level: None,
                    warning: None,
                    error: None,
                };
            }

            let (fallback_model, fallback_thinking, fallback_warning) = parse_model_pattern(cli_model_str, &available_models, false);
            if let Some(m) = fallback_model {
                return ResolveResult {
                    model: Some(m),
                    thinking_level: fallback_thinking,
                    warning: fallback_warning,
                    error: None,
                };
            }
        }

        if let Some(p) = &provider {
            let mut fallback_pattern = pattern.clone();
            let mut fallback_thinking = None;
            
            let mut last_colon_opt = None;
            if cli_thinking.is_none() {
                last_colon_opt = pattern.rfind(':');
            }
            if let Some(last_colon) = last_colon_opt {
                let suffix = &pattern[last_colon + 1..];
                if let Some(level) = parse_thinking_level(suffix) {
                    fallback_pattern = pattern[..last_colon].to_string();
                    fallback_thinking = Some(level);
                }
            }

            let fallback_model = build_fallback_model(p, &fallback_pattern, &available_models);
            if let Some(mut fm) = fallback_model {
                let requested_thinking = cli_thinking.or(fallback_thinking);
                if requested_thinking.unwrap_or(pi_ai::types::ModelThinkingLevel::Off) != pi_ai::types::ModelThinkingLevel::Off {
                    fm.reasoning = true;
                }
                let fallback_warning = if let Some(w) = &warning {
                    format!("{} Model \"{}\" not found for provider \"{}\". Using custom model id.", w, fallback_pattern, p)
                } else {
                    format!("Model \"{}\" not found for provider \"{}\". Using custom model id.", fallback_pattern, p)
                };
                return ResolveResult {
                    model: Some(fm),
                    thinking_level: fallback_thinking,
                    warning: Some(fallback_warning),
                    error: None,
                };
            }
        }

        let display = if let Some(p) = &provider {
            format!("{}/{}", p, pattern)
        } else {
            cli_model_str.to_string()
        };

        ResolveResult {
            model: None,
            thinking_level: None,
            warning: None,
            error: Some(format!("Model \"{}\" not found. Use --list-models to see available models.", display)),
        }
    }
}

fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let bytes = id.as_bytes();
    if bytes.len() >= 9 {
        let suffix = &bytes[bytes.len() - 9..];
        if suffix[0] == b'-' && suffix[1..9].iter().all(|&c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

fn find_exact_model_reference_match(
    reference: &str,
    models: &[pi_ai::types::Model],
) -> Option<pi_ai::types::Model> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_lowercase();

    let mut canonical_matches = Vec::new();
    for m in models {
        let full_id = format!("{}/{}", m.provider, m.id).to_lowercase();
        if full_id == normalized {
            canonical_matches.push(m.clone());
        }
    }
    if canonical_matches.len() == 1 {
        return Some(canonical_matches[0].clone());
    }
    if canonical_matches.len() > 1 {
        return None;
    }

    if let Some(slash_idx) = trimmed.find('/') {
        let provider = trimmed[..slash_idx].trim().to_lowercase();
        let model_id = trimmed[slash_idx + 1..].trim().to_lowercase();
        if !provider.is_empty() && !model_id.is_empty() {
            let mut provider_matches = Vec::new();
            for m in models {
                if m.provider.to_lowercase() == provider && m.id.to_lowercase() == model_id {
                    provider_matches.push(m.clone());
                }
            }
            if provider_matches.len() == 1 {
                return Some(provider_matches[0].clone());
            }
            if provider_matches.len() > 1 {
                return None;
            }
        }
    }

    let mut id_matches = Vec::new();
    for m in models {
        if m.id.to_lowercase() == normalized {
            id_matches.push(m.clone());
        }
    }
    if id_matches.len() == 1 {
        return Some(id_matches[0].clone());
    }
    None
}

fn try_match_model(pattern: &str, models: &[pi_ai::types::Model]) -> Option<pi_ai::types::Model> {
    if let Some(exact) = find_exact_model_reference_match(pattern, models) {
        return Some(exact);
    }

    let pattern_lower = pattern.to_lowercase();
    let mut matches = Vec::new();
    for m in models {
        if m.id.to_lowercase().contains(&pattern_lower) || m.name.to_lowercase().contains(&pattern_lower) {
            matches.push(m.clone());
        }
    }

    if matches.is_empty() {
        return None;
    }

    let mut aliases = Vec::new();
    let mut dated = Vec::new();
    for m in matches {
        if is_alias(&m.id) {
            aliases.push(m);
        } else {
            dated.push(m);
        }
    }

    if !aliases.is_empty() {
        aliases.sort_by(|a, b| b.id.cmp(&a.id));
        Some(aliases[0].clone())
    } else {
        dated.sort_by(|a, b| b.id.cmp(&a.id));
        Some(dated[0].clone())
    }
}

fn parse_thinking_level(suffix: &str) -> Option<pi_ai::types::ModelThinkingLevel> {
    match suffix.to_lowercase().as_str() {
        "off" => Some(pi_ai::types::ModelThinkingLevel::Off),
        "minimal" => Some(pi_ai::types::ModelThinkingLevel::Minimal),
        "low" => Some(pi_ai::types::ModelThinkingLevel::Low),
        "medium" => Some(pi_ai::types::ModelThinkingLevel::Medium),
        "high" => Some(pi_ai::types::ModelThinkingLevel::High),
        "xhigh" => Some(pi_ai::types::ModelThinkingLevel::Xhigh),
        "max" => Some(pi_ai::types::ModelThinkingLevel::Max),
        _ => None,
    }
}

fn parse_model_pattern(
    pattern: &str,
    models: &[pi_ai::types::Model],
    allow_invalid_fallback: bool,
) -> (Option<pi_ai::types::Model>, Option<pi_ai::types::ModelThinkingLevel>, Option<String>) {
    if let Some(exact) = try_match_model(pattern, models) {
        return (Some(exact), None, None);
    }

    let last_colon = pattern.rfind(':');
    let Some(colon_idx) = last_colon else {
        return (None, None, None);
    };

    let prefix = &pattern[..colon_idx];
    let suffix = &pattern[colon_idx + 1..];

    if let Some(level) = parse_thinking_level(suffix) {
        let (model, inner_level, warning) = parse_model_pattern(prefix, models, allow_invalid_fallback);
        if model.is_some() {
            return (
                model,
                if warning.is_some() { None } else { Some(level) },
                warning,
            );
        }
        (model, inner_level, warning)
    } else {
        if !allow_invalid_fallback {
            return (None, None, None);
        }

        let (model, inner_level, warning) = parse_model_pattern(prefix, models, allow_invalid_fallback);
        if model.is_some() {
            return (
                model,
                None,
                Some(format!("Invalid thinking level \"{}\" in pattern \"{}\". Using default instead.", suffix, pattern)),
            );
        }
        (model, inner_level, warning)
    }
}

fn default_model_id(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "amazon-bedrock" => "us.anthropic.claude-opus-4-6-v1",
        "ant-ling" => "Ring-2.6-1T",
        "anthropic" => "claude-opus-4-8",
        "openai" => "gpt-5.5",
        "azure-openai-responses" => "gpt-5.4",
        "openai-codex" => "gpt-5.5",
        "radius" => "auto",
        "nvidia" => "nvidia/nemotron-3-super-120b-a12b",
        "deepseek" => "deepseek-v4-pro",
        "google" => "gemini-3.1-pro-preview",
        "google-vertex" => "gemini-3.1-pro-preview",
        "github-copilot" => "gpt-5.4",
        "openrouter" => "moonshotai/kimi-k2.6",
        "vercel-ai-gateway" => "zai/glm-5.1",
        "xai" => "grok-4.20-0309-reasoning",
        "groq" => "openai/gpt-oss-120b",
        "cerebras" => "zai-glm-4.7",
        "zai" => "glm-5.1",
        "zai-coding-cn" => "glm-5.1",
        "mistral" => "devstral-medium-latest",
        "minimax" => "MiniMax-M2.7",
        "minimax-cn" => "MiniMax-M2.7",
        "moonshotai" => "kimi-k2.6",
        "moonshotai-cn" => "kimi-k2.6",
        "huggingface" => "moonshotai/Kimi-K2.6",
        "fireworks" => "accounts/fireworks/models/kimi-k2p6",
        "together" => "moonshotai/Kimi-K2.6",
        "opencode" => "kimi-k2.6",
        "opencode-go" => "kimi-k2.6",
        "kimi-coding" => "kimi-for-coding",
        "cloudflare-workers-ai" => "@cf/moonshotai/kimi-k2.6",
        "cloudflare-ai-gateway" => "workers-ai/@cf/moonshotai/kimi-k2.6",
        "xiaomi" => "mimo-v2.5-pro",
        "xiaomi-token-plan-cn" => "mimo-v2.5-pro",
        "xiaomi-token-plan-ams" => "mimo-v2.5-pro",
        "xiaomi-token-plan-sgp" => "mimo-v2.5-pro",
        _ => return None,
    })
}

fn build_fallback_model(
    provider: &str,
    model_id: &str,
    models: &[pi_ai::types::Model],
) -> Option<pi_ai::types::Model> {
    let provider_models: Vec<_> = models.iter().filter(|m| m.provider == provider).collect();
    if provider_models.is_empty() {
        return None;
    }

    let default_id = default_model_id(provider);
    let base_model = if let Some(def_id) = default_id {
        provider_models.iter().find(|m| m.id == def_id).copied().unwrap_or(provider_models[0])
    } else {
        provider_models[0]
    };

    let mut model = base_model.clone();
    model.id = model_id.to_string();
    model.name = model_id.to_string();
    Some(model)
}

fn apply_model_override(mut model: pi_ai::types::Model, r#override: &ModelOverride) -> pi_ai::types::Model {
    if let Some(name) = &r#override.name {
        model.name = name.clone();
    }
    if let Some(reasoning) = r#override.reasoning {
        model.reasoning = reasoning;
    }
    if let Some(thinking_level_map) = &r#override.thinking_level_map {
        let mut base_map = model.thinking_level_map.unwrap_or_default();
        for (k, v) in thinking_level_map {
            base_map.insert(*k, v.clone());
        }
        model.thinking_level_map = Some(base_map);
    }
    if let Some(input) = &r#override.input {
        model.input = input.clone();
    }
    if let Some(context_window) = r#override.context_window {
        model.context_window = context_window;
    }
    if let Some(max_tokens) = r#override.max_tokens {
        model.max_tokens = max_tokens;
    }

    if let Some(cost) = &r#override.cost {
        if let Some(input) = cost.input {
            model.cost.input = input;
        }
        if let Some(output) = cost.output {
            model.cost.output = output;
        }
        if let Some(cache_read) = cost.cache_read {
            model.cost.cache_read = cache_read;
        }
        if let Some(cache_write) = cost.cache_write {
            model.cost.cache_write = cache_write;
        }
        if let Some(tiers) = &cost.tiers {
            model.cost.tiers = tiers.clone();
        }
    }

    model.compat = merge_json_values(model.compat, r#override.compat.clone());
    model
}

fn get_built_in_defaults(provider_name: &str) -> Option<(String, String)> {
    for entry in pi_ai::models_generated::MODELS {
        if let (true, Ok(model)) = (entry.provider == provider_name, entry.to_model()) {
            return Some((model.api.0, model.base_url));
        }
    }
    None
}

fn validate_config(config: &ModelsConfig) -> Result<(), String> {
    let mut built_in_providers = std::collections::HashSet::new();
    for entry in pi_ai::models_generated::MODELS {
        built_in_providers.insert(entry.provider);
    }

    for (provider_name, provider_config) in &config.providers {
        let is_built_in = built_in_providers.contains(provider_name.as_str());
        let has_provider_api = provider_config.api.is_some();
        let models = provider_config.models.as_deref().unwrap_or(&[]);
        let has_model_overrides = provider_config.model_overrides.as_ref().map(|mo| !mo.is_empty()).unwrap_or(false);

        if provider_config.oauth.is_some() && provider_config.base_url.is_none() {
            return Err(format!("Provider {}: \"baseUrl\" is required when \"oauth\" is set.", provider_name));
        }

        if models.is_empty() && provider_config.oauth.is_none() {
            if provider_config.base_url.is_none()
                && provider_config.headers.is_none()
                && provider_config.compat.is_none()
                && !has_model_overrides
            {
                return Err(format!(
                    "Provider {}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\".",
                    provider_name
                ));
            }
        } else if !is_built_in && provider_config.base_url.is_none() {
            return Err(format!("Provider {}: \"baseUrl\" is required when defining custom models.", provider_name));
        }

        for model_def in models {
            let has_model_api = model_def.api.is_some();

            if !has_provider_api && !has_model_api && !is_built_in {
                return Err(format!(
                    "Provider {}, model {}: no \"api\" specified. Set at provider or model level.",
                    provider_name, model_def.id
                ));
            }

            if model_def.id.is_empty() {
                return Err(format!("Provider {}: model missing \"id\"", provider_name));
            }

            if model_def.context_window == Some(0) {
                return Err(format!("Provider {}, model {}: invalid contextWindow", provider_name, model_def.id));
            }

            if model_def.max_tokens == Some(0) {
                return Err(format!("Provider {}, model {}: invalid maxTokens", provider_name, model_def.id));
            }
        }
    }
    Ok(())
}

fn strip_json_comments(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;

    // First pass: strip // comments
    while i < chars.len() {
        if chars[i] == '"' {
            out.push('"');
            i += 1;
            while i < chars.len() {
                if chars[i] == '"' {
                    out.push('"');
                    i += 1;
                    break;
                }
                if chars[i] == '\\' && i + 1 < chars.len() {
                    out.push('\\');
                    out.push(chars[i + 1]);
                    i += 2;
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
            }
        } else if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            i += 2;
            while i < chars.len() && chars[i] != '\n' && chars[i] != '\r' {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }

    // Second pass: strip trailing commas before } or ]
    let chars2: Vec<char> = out.chars().collect();
    let mut out2 = String::new();
    let mut j = 0;

    while j < chars2.len() {
        if chars2[j] == '"' {
            out2.push('"');
            j += 1;
            while j < chars2.len() {
                if chars2[j] == '"' {
                    out2.push('"');
                    j += 1;
                    break;
                }
                if chars2[j] == '\\' && j + 1 < chars2.len() {
                    out2.push('\\');
                    out2.push(chars2[j + 1]);
                    j += 2;
                } else {
                    out2.push(chars2[j]);
                    j += 1;
                }
            }
        } else if chars2[j] == ',' {
            let mut peek = j + 1;
            while peek < chars2.len() && chars2[peek].is_ascii_whitespace() {
                peek += 1;
            }
            if peek < chars2.len() && (chars2[peek] == '}' || chars2[peek] == ']') {
                j += 1;
            } else {
                out2.push(',');
                j += 1;
            }
        } else {
            out2.push(chars2[j]);
            j += 1;
        }
    }

    out2
}

fn merge_json_values(base: Option<Value>, r#override: Option<Value>) -> Option<Value> {
    match (base, r#override) {
        (None, None) => None,
        (Some(b), None) => Some(b),
        (None, Some(o)) => Some(o),
        (Some(b), Some(o)) => {
            match (b, o) {
                (Value::Object(mut b_map), Value::Object(o_map)) => {
                    for (k, v) in o_map {
                        if k == "openRouterRouting" || k == "vercelGatewayRouting" || k == "chatTemplateKwargs" {
                            let merged = merge_json_values(b_map.remove(&k), Some(v));
                            if let Some(m) = merged {
                                b_map.insert(k, m);
                            }
                        } else {
                            b_map.insert(k, v);
                        }
                    }
                    Some(Value::Object(b_map))
                }
                (_, o_val) => Some(o_val),
            }
        }
    }
}
