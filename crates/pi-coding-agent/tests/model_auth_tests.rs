use std::collections::HashMap;
use std::fs;
use tempfile::TempDir;

use pi_coding_agent::{
    AuthStorage, ModelRegistry,
    ProviderConfigInput, ModelDefinition,
    clear_config_value_cache,
};
use pi_coding_agent::resolve_config_value::{
    resolve_config_value, resolve_config_value_uncached, resolve_config_value_or_throw,
    resolve_headers,
};
use pi_ai::auth::{Credential, ApiKeyCredential, OAuthCredential};

static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvGuard {
    original_values: HashMap<String, Option<String>>,
}

impl EnvGuard {
    fn new(vars: &[&str]) -> Self {
        let mut original_values = HashMap::new();
        for &var in vars {
            let val = std::env::var(var).ok();
            original_values.insert(var.to_string(), val);
        }
        Self { original_values }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (var, val) in &self.original_values {
            unsafe {
                match val {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

#[tokio::test]
async fn test_precedence_and_runtime_overrides() {
    let _lock = ENV_MUTEX.lock().await;
    let _env_guard = EnvGuard::new(&["ANTHROPIC_API_KEY"]);

    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");

    // Setup AuthStorage
    let mut auth_storage = AuthStorage::new(auth_path.clone());

    // 1. Initially no auth is configured
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    assert_eq!(auth_storage.get_api_key("anthropic", true).await.unwrap(), None);

    // 2. Fallback to Env var
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "env-key-value");
    }
    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        Some("env-key-value".to_string())
    );
    // If include_fallback is false, env should be ignored
    assert_eq!(auth_storage.get_api_key("anthropic", false).await.unwrap(), None);

    // 3. Stored API key (in auth.json) takes precedence over Env var
    let credential = Credential::ApiKey(ApiKeyCredential {
        key: Some("stored-key-value".to_string()),
        env: None,
        extra: HashMap::new(),
    });
    auth_storage.set("anthropic", credential).await.unwrap();

    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        Some("stored-key-value".to_string())
    );

    // 4. Runtime override takes highest priority
    auth_storage.set_runtime_api_key("anthropic".to_string(), "runtime-key-value".to_string());
    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        Some("runtime-key-value".to_string())
    );

    // 5. Removing runtime override falls back to stored key
    auth_storage.remove_runtime_api_key("anthropic");
    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        Some("stored-key-value".to_string())
    );
}

#[tokio::test]
async fn test_env_file_auth_interpolation() {
    let _lock = ENV_MUTEX.lock().await;
    let _env_guard = EnvGuard::new(&["MY_SECRET_KEY_VAR"]);

    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let auth_storage = AuthStorage::new(auth_path);

    unsafe {
        std::env::set_var("MY_SECRET_KEY_VAR", "interpolated-secret");
    }

    // 1. Literal env var reference
    let credential = Credential::ApiKey(ApiKeyCredential {
        key: Some("$MY_SECRET_KEY_VAR".to_string()),
        env: None,
        extra: HashMap::new(),
    });
    auth_storage.set("openai", credential).await.unwrap();

    assert_eq!(
        auth_storage.get_api_key("openai", true).await.unwrap(),
        Some("interpolated-secret".to_string())
    );

    // 2. Scoped env in credential takes precedence
    let mut scope_env = HashMap::new();
    scope_env.insert("MY_SECRET_KEY_VAR".to_string(), "scoped-secret".to_string());
    let credential_scoped = Credential::ApiKey(ApiKeyCredential {
        key: Some("${MY_SECRET_KEY_VAR}".to_string()),
        env: Some(scope_env),
        extra: HashMap::new(),
    });
    auth_storage.set("openai", credential_scoped).await.unwrap();

    assert_eq!(
        auth_storage.get_api_key("openai", true).await.unwrap(),
        Some("scoped-secret".to_string())
    );
}

#[tokio::test]
async fn test_custom_model_decode_and_overrides() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let models_json_path = temp_dir.path().join("models.json");

    let auth_storage = AuthStorage::new(auth_path);

    // Write a models.json with comments, custom models, and overrides
    let models_json_content = r#"{
        // This is a comment at the top
        "providers": {
            "custom-ollama": {
                "baseUrl": "http://localhost:11434",
                "api": "openai-completions",
                "models": [
                    {
                        "id": "llama3",
                        "name": "Llama 3 Custom",
                        "cost": {
                            "input": 0.1,
                            "output": 0.2,
                            "cacheRead": 0.0,
                            "cacheWrite": 0.0
                        },
                        "contextWindow": 8192,
                        "maxTokens": 2048,
                        "reasoning": true
                    }
                ]
            },
            "openai": {
                // Override built-in model config
                "modelOverrides": {
                    "gpt-4o": {
                        "name": "GPT-4o Overridden",
                        "contextWindow": 250000,
                        "cost": {
                            "input": 1.25,
                            "output": 3.75
                        }
                    }
                }
            }
        }
    }"#;

    fs::write(&models_json_path, models_json_content).unwrap();

    let registry = ModelRegistry::create(auth_storage, models_json_path);

    // 1. Verify custom model is decoded correctly
    let custom_model = registry.find("custom-ollama", "llama3").expect("should find custom model");
    assert_eq!(custom_model.name, "Llama 3 Custom");
    assert_eq!(custom_model.base_url, "http://localhost:11434");
    assert_eq!(custom_model.api.0, "openai-completions");
    assert_eq!(custom_model.context_window, 8192);
    assert_eq!(custom_model.max_tokens, 2048);
    assert!(custom_model.reasoning);

    // 2. Verify model overrides are applied
    let overridden_model = registry.find("openai", "gpt-4o").expect("should find built-in model");
    assert_eq!(overridden_model.name, "GPT-4o Overridden");
    assert_eq!(overridden_model.context_window, 250000);
    assert_eq!(overridden_model.cost.input, 1.25);
    assert_eq!(overridden_model.cost.output, 3.75);
}

#[tokio::test]
async fn test_availability() {
    let _lock = ENV_MUTEX.lock().await;
    let _env_guard = EnvGuard::new(&["ANTHROPIC_API_KEY"]);

    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let auth_storage = AuthStorage::new(auth_path);

    // Read live state of available models initially
    let registry = ModelRegistry::in_memory(auth_storage);
    let initial_count = registry.get_available().await.len();

    // Configure auth for anthropic
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "env-key");
    }

    let final_available = registry.get_available().await;
    // Availability count must increase
    assert!(final_available.len() > initial_count);
    assert!(final_available.iter().any(|m| m.provider == "anthropic"));
}

#[tokio::test]
async fn test_no_secret_leakage() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let auth_storage = AuthStorage::new(auth_path.clone());

    // 1. Store a real-looking API key secret
    let secret_key = "sk-proj-super-secret-key-12345-never-leak-this";
    let api_cred = Credential::ApiKey(ApiKeyCredential {
        key: Some(secret_key.to_string()),
        env: None,
        extra: HashMap::new(),
    });
    auth_storage.set("openai", api_cred).await.unwrap();

    // 2. Store a real-looking OAuth token secret
    let oauth_token = "oauth-access-token-98765-must-be-hidden";
    let oauth_cred = Credential::OAuth(OAuthCredential {
        access: oauth_token.to_string(),
        refresh: "refresh-token".to_string(),
        expires: i64::MAX, // safely in future
        extra: HashMap::new(),
    });
    auth_storage.set("anthropic", oauth_cred).await.unwrap();

    // 3. Verify get_auth_status output and formatting does NOT leak any secrets
    let api_status = auth_storage.get_auth_status("openai").await.unwrap();
    assert!(api_status.configured);
    assert_eq!(api_status.source, Some("stored".to_string()));

    let api_debug = format!("{:?}", api_status);
    let api_json = serde_json::to_string(&api_status).unwrap();
    assert!(!api_debug.contains(secret_key));
    assert!(!api_json.contains(secret_key));

    let oauth_status = auth_storage.get_auth_status("anthropic").await.unwrap();
    assert!(oauth_status.configured);
    assert_eq!(oauth_status.source, Some("stored".to_string()));

    let oauth_debug = format!("{:?}", oauth_status);
    let oauth_json = serde_json::to_string(&oauth_status).unwrap();
    assert!(!oauth_debug.contains(oauth_token));
    assert!(!oauth_json.contains(oauth_token));
}

#[tokio::test]
async fn test_model_resolution() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let auth_storage = AuthStorage::new(auth_path.clone());

    // Let's create an in-memory registry containing all built-in models
    let registry = ModelRegistry::in_memory(auth_storage);

    // 1. Resolve exact match
    let res = registry.resolve(Some("anthropic"), Some("claude-opus-4-8"), None).await;
    assert_eq!(res.error, None);
    let matched = res.model.unwrap();
    assert_eq!(matched.provider, "anthropic");
    assert_eq!(matched.id, "claude-opus-4-8");

    // 2. Resolve fuzzy match (substring) scoped to provider to avoid collisions
    let res_fuzzy = registry.resolve(Some("anthropic"), Some("sonnet-5"), None).await;
    assert_eq!(res_fuzzy.error, None);
    let matched_fuzzy = res_fuzzy.model.unwrap();
    assert_eq!(matched_fuzzy.id, "claude-sonnet-5");

    // 3. Resolve thinking level suffix in model pattern (scoped to provider)
    let res_thinking = registry.resolve(Some("anthropic"), Some("claude-opus-4-8:high"), None).await;
    assert_eq!(res_thinking.error, None);
    assert_eq!(res_thinking.thinking_level, Some(pi_ai::types::ModelThinkingLevel::High));
    assert_eq!(res_thinking.model.unwrap().id, "claude-opus-4-8");

    // 3b. Test all thinking levels resolve correctly
    for level_str in &["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
        let pattern = format!("claude-opus-4-8:{}", level_str);
        let res_lvl = registry.resolve(Some("anthropic"), Some(&pattern), None).await;
        assert_eq!(res_lvl.error, None);
        let expected_lvl = match *level_str {
            "off" => pi_ai::types::ModelThinkingLevel::Off,
            "minimal" => pi_ai::types::ModelThinkingLevel::Minimal,
            "low" => pi_ai::types::ModelThinkingLevel::Low,
            "medium" => pi_ai::types::ModelThinkingLevel::Medium,
            "high" => pi_ai::types::ModelThinkingLevel::High,
            "xhigh" => pi_ai::types::ModelThinkingLevel::Xhigh,
            "max" => pi_ai::types::ModelThinkingLevel::Max,
            _ => unreachable!(),
        };
        assert_eq!(res_lvl.thinking_level, Some(expected_lvl));
    }

    // 4. Fallback custom model when not found but provider is valid
    let res_fallback = registry.resolve(Some("openai"), Some("gpt-nonexistent"), None).await;
    assert_eq!(res_fallback.error, None);
    let fallback = res_fallback.model.unwrap();
    assert_eq!(fallback.provider, "openai");
    assert_eq!(fallback.id, "gpt-nonexistent");
    assert!(res_fallback.warning.unwrap().contains("gpt-nonexistent"));

    // 5. Unknown provider error
    let res_unknown_provider = registry.resolve(Some("invalid-provider"), Some("llama3"), None).await;
    assert!(res_unknown_provider.error.unwrap().contains("Unknown provider"));

    // 6. Unknown model error (when provider not matched/inferred)
    let res_unknown_model = registry.resolve(None, Some("nonexistent-model-completely-random"), None).await;
    assert!(res_unknown_model.error.unwrap().contains("not found"));

    // 7. Resolve Unicode model ID
    let custom_config = ProviderConfigInput {
        name: None,
        base_url: Some("http://localhost".to_string()),
        api_key: Some("test-key".to_string()),
        api: Some("openai-responses".to_string()),
        headers: None,
        auth_header: None,
        oauth: None,
        models: Some(vec![ModelDefinition {
            id: "모델-id-🚀".to_string(),
            name: Some("Korean Model Rocket".to_string()),
            api: None,
            base_url: None,
            reasoning: None,
            thinking_level_map: None,
            input: None,
            cost: None,
            context_window: None,
            max_tokens: None,
            headers: None,
            compat: None,
        }]),
    };

    let mut registry_with_unicode = ModelRegistry::in_memory(AuthStorage::new(auth_path.clone()));
    registry_with_unicode.register_provider("custom-korean".to_string(), custom_config).unwrap();

    let res_unicode = registry_with_unicode.resolve(Some("custom-korean"), Some("모델-id-🚀"), None).await;
    assert_eq!(res_unicode.error, None);
    let matched_unicode = res_unicode.model.unwrap();
    assert_eq!(matched_unicode.id, "모델-id-🚀");
    assert_eq!(matched_unicode.name, "Korean Model Rocket");
}

#[tokio::test]
async fn test_custom_model_rejections() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let models_json_path = temp_dir.path().join("models.json");

    // 1. Invalid oauth provider type in custom models config
    let content_invalid_oauth = r#"{
        "providers": {
            "custom-provider": {
                "baseUrl": "http://localhost",
                "oauth": "invalid_oauth_type",
                "models": [
                    {
                        "id": "my-model",
                        "api": "openai-responses"
                    }
                ]
            }
        }
    }"#;

    fs::write(&models_json_path, content_invalid_oauth).unwrap();
    let registry = ModelRegistry::create(AuthStorage::new(auth_path.clone()), models_json_path.clone());
    assert!(registry.get_error().is_some());
    assert!(registry.get_error().unwrap().contains("unknown variant `invalid_oauth_type`"));

    // 2. Partial/incomplete cost configuration in custom model definition (missing output)
    // 2. Partial/incomplete cost configuration in custom model definition (missing output)
    let content_partial_cost = r#"{
        "providers": {
            "custom-provider": {
                "baseUrl": "http://localhost",
                "models": [
                    {
                        "id": "my-model",
                        "api": "openai-responses",
                        "cost": {
                            "input": 0.5
                            // missing output, cacheRead, cacheWrite which are required for DefinitionCost
                        }
                    }
                ]
            }
        }
    }"#;

    fs::write(&models_json_path, content_partial_cost).unwrap();
    let registry2 = ModelRegistry::create(AuthStorage::new(auth_path.clone()), models_json_path.clone());
    assert!(registry2.get_error().is_some());
    assert!(registry2.get_error().unwrap().contains("missing field `output`"));

    // 3. Block comments /* ... */ are not stripped and fail to parse
    let content_block_comment = r#"{
        "providers": {
            "custom-provider": {
                "baseUrl": "http://localhost",
                /* block comment */
                "models": [
                    {
                        "id": "my-model",
                        "api": "openai-responses"
                    }
                ]
            }
        }
    }"#;

    fs::write(&models_json_path, content_block_comment).unwrap();
    let registry3 = ModelRegistry::create(AuthStorage::new(auth_path), models_json_path);
    assert!(registry3.get_error().is_some());

}

#[tokio::test]
async fn test_oauth_auth_resolution() {
    let _lock = ENV_MUTEX.lock().await;
    let _env_guard = EnvGuard::new(&["ANTHROPIC_API_KEY"]);

    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let auth_storage = AuthStorage::new(auth_path);

    // Env fallback initially
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "env-anthropic-key");
    }
    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        Some("env-anthropic-key".to_string())
    );

    // Stored non-expired OAuth credential for anthropic
    let oauth_cred = Credential::OAuth(OAuthCredential {
        access: "my-oauth-access-token".to_string(),
        refresh: "my-refresh-token".to_string(),
        expires: i64::MAX,
        extra: HashMap::new(),
    });
    auth_storage.set("anthropic", oauth_cred).await.unwrap();

    // Stored OAuth key takes precedence over Env var
    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        Some("my-oauth-access-token".to_string())
    );
}

#[test]
fn test_config_resolver_commands_and_headers() {
    clear_config_value_cache();

    // 1. Commands
    let res = resolve_config_value("!echo my-cmd-output", None);
    assert_eq!(res, Some("my-cmd-output".to_string()));

    // Cache lookup should succeed
    let res_cached = resolve_config_value("!echo my-cmd-output", None);
    assert_eq!(res_cached, Some("my-cmd-output".to_string()));

    // Uncached lookup
    let res_uncached = resolve_config_value_uncached("!echo my-cmd-output-2", None);
    assert_eq!(res_uncached, Some("my-cmd-output-2".to_string()));

    // 2. Templates and errors
    let err = resolve_config_value_or_throw("$MISSING_ENV_VAR", "my key", None);
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("MISSING_ENV_VAR"));

    // 3. Header resolution (JS-falsy filtering)
    let mut headers = HashMap::new();
    headers.insert("HeaderA".to_string(), "LiteralValue".to_string());
    headers.insert("HeaderB".to_string(), "$MISSING_VAR".to_string()); // resolves to None, so skipped in resolve_headers
    headers.insert("HeaderC".to_string(), "".to_string()); // resolves to "" which is JS-falsy, so skipped

    let resolved = resolve_headers(Some(&headers), None);
    let r = resolved.unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r.get("HeaderA"), Some(&"LiteralValue".to_string()));
}
