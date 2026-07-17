use std::collections::HashMap;
use std::fs;
use tempfile::TempDir;

use pi_ai::auth::{ApiKeyCredential, Credential, OAuthCredential};
use pi_coding_agent::resolve_config_value::{
    resolve_config_value, resolve_config_value_or_throw, resolve_config_value_uncached,
    resolve_headers,
};
use pi_coding_agent::{
    AuthStorage, ModelDefinition, ModelRegistry, ProviderConfigInput, clear_config_value_cache,
};

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
    let auth_storage = AuthStorage::new(auth_path.clone());

    // 1. Initially no auth is configured
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        None
    );

    // 2. Fallback to Env var
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "env-key-value");
    }
    assert_eq!(
        auth_storage.get_api_key("anthropic", true).await.unwrap(),
        Some("env-key-value".to_string())
    );
    // If include_fallback is false, env should be ignored
    assert_eq!(
        auth_storage.get_api_key("anthropic", false).await.unwrap(),
        None
    );

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

    let registry = ModelRegistry::create(std::sync::Arc::new(auth_storage), models_json_path);

    // 1. Verify custom model is decoded correctly
    let custom_model = registry
        .find("custom-ollama", "llama3")
        .expect("should find custom model");
    assert_eq!(custom_model.name, "Llama 3 Custom");
    assert_eq!(custom_model.base_url, "http://localhost:11434");
    assert_eq!(custom_model.api.0, "openai-completions");
    assert_eq!(custom_model.context_window, 8192);
    assert_eq!(custom_model.max_tokens, 2048);
    assert!(custom_model.reasoning);

    // 2. Verify model overrides are applied
    let overridden_model = registry
        .find("openai", "gpt-4o")
        .expect("should find built-in model");
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
    let registry = ModelRegistry::in_memory(std::sync::Arc::new(auth_storage));
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
async fn test_shared_auth_storage_runtime_override_visible_to_registry() {
    let _lock = ENV_MUTEX.lock().await;
    let _env_guard = EnvGuard::new(&["ANTHROPIC_API_KEY"]);
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let auth_storage = std::sync::Arc::new(AuthStorage::new(auth_path));

    // Registry holds the SAME live instance, not a copy.
    let registry = ModelRegistry::in_memory(auth_storage.clone());
    let model = registry
        .find("anthropic", "claude-opus-4-8")
        .expect("builtin model")
        .clone();
    assert!(!registry.has_configured_auth(&model).await);

    // A runtime override set through the shared handle is immediately
    // visible to the registry (the --api-key flow).
    auth_storage.set_runtime_api_key("anthropic".to_string(), "runtime-shared-key".to_string());
    assert!(registry.has_configured_auth(&model).await);
    assert_eq!(
        registry.get_api_key_for_provider("anthropic").await,
        Some("runtime-shared-key".to_string())
    );

    auth_storage.remove_runtime_api_key("anthropic");
    assert!(!registry.has_configured_auth(&model).await);
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
    let registry = ModelRegistry::in_memory(std::sync::Arc::new(auth_storage));

    // 1. Resolve exact match
    let res = registry
        .resolve(Some("anthropic"), Some("claude-opus-4-8"), None)
        .await;
    assert_eq!(res.error, None);
    let matched = res.model.unwrap();
    assert_eq!(matched.provider, "anthropic");
    assert_eq!(matched.id, "claude-opus-4-8");

    // 2. Resolve fuzzy match (substring) scoped to provider to avoid collisions
    let res_fuzzy = registry
        .resolve(Some("anthropic"), Some("sonnet-5"), None)
        .await;
    assert_eq!(res_fuzzy.error, None);
    let matched_fuzzy = res_fuzzy.model.unwrap();
    assert_eq!(matched_fuzzy.id, "claude-sonnet-5");

    // 3. Resolve thinking level suffix in model pattern (scoped to provider)
    let res_thinking = registry
        .resolve(Some("anthropic"), Some("claude-opus-4-8:high"), None)
        .await;
    assert_eq!(res_thinking.error, None);
    assert_eq!(
        res_thinking.thinking_level,
        Some(pi_ai::types::ModelThinkingLevel::High)
    );
    assert_eq!(res_thinking.model.unwrap().id, "claude-opus-4-8");

    // 3b. Test all thinking levels resolve correctly
    for level_str in &["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
        let pattern = format!("claude-opus-4-8:{}", level_str);
        let res_lvl = registry
            .resolve(Some("anthropic"), Some(&pattern), None)
            .await;
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
    let res_fallback = registry
        .resolve(Some("openai"), Some("gpt-nonexistent"), None)
        .await;
    assert_eq!(res_fallback.error, None);
    let fallback = res_fallback.model.unwrap();
    assert_eq!(fallback.provider, "openai");
    assert_eq!(fallback.id, "gpt-nonexistent");
    assert!(res_fallback.warning.unwrap().contains("gpt-nonexistent"));

    // 5. Unknown provider error
    let res_unknown_provider = registry
        .resolve(Some("invalid-provider"), Some("llama3"), None)
        .await;
    assert!(
        res_unknown_provider
            .error
            .unwrap()
            .contains("Unknown provider")
    );

    // 6. Unknown model error (when provider not matched/inferred)
    let res_unknown_model = registry
        .resolve(None, Some("nonexistent-model-completely-random"), None)
        .await;
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

    let mut registry_with_unicode =
        ModelRegistry::in_memory(std::sync::Arc::new(AuthStorage::new(auth_path.clone())));
    registry_with_unicode
        .register_provider("custom-korean".to_string(), custom_config)
        .unwrap();

    let res_unicode = registry_with_unicode
        .resolve(Some("custom-korean"), Some("모델-id-🚀"), None)
        .await;
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
    let registry = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path.clone())),
        models_json_path.clone(),
    );
    assert!(registry.get_error().is_some());
    assert!(
        registry
            .get_error()
            .unwrap()
            .contains("unknown variant `invalid_oauth_type`")
    );

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
    let registry2 = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path.clone())),
        models_json_path.clone(),
    );
    assert!(registry2.get_error().is_some());
    assert!(
        registry2
            .get_error()
            .unwrap()
            .contains("missing field `output`")
    );

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
    let registry3 = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path)),
        models_json_path,
    );
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

fn oauth_credential(access: &str, expires: i64, extra: serde_json::Value) -> Credential {
    Credential::OAuth(OAuthCredential {
        access: access.to_owned(),
        refresh: "refresh-token".to_owned(),
        expires,
        extra: extra
            .as_object()
            .expect("OAuth extra must be an object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}

fn write_credentials(path: &std::path::Path, credentials: &[(&str, Credential)]) {
    let values = credentials
        .iter()
        .map(|(provider, credential)| ((*provider).to_owned(), credential.clone()))
        .collect::<HashMap<_, _>>();
    fs::write(path, serde_json::to_vec_pretty(&values).unwrap()).unwrap();
}

#[test]
fn oauth_credentials_transform_builtin_catalogs_after_merge() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let models_path = temp_dir.path().join("models.json");
    fs::write(&models_path, r#"{"providers": {}}"#).unwrap();

    let baseline = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path.clone())),
        models_path.clone(),
    );
    let copilot_ids = baseline
        .get_all()
        .iter()
        .filter(|model| model.provider == "github-copilot")
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    assert!(copilot_ids.len() > 1);
    let kept_copilot_id = copilot_ids[0].clone();

    let radius_extra = serde_json::json!({"gatewayConfig": {
        "baseUrl": "https://radius.example/v1",
        "models": [{"id": "radius-dynamic", "name": "Radius Dynamic", "reasoning": true,
            "input": ["text"], "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.0, "cacheWrite": 0.0},
            "contextWindow": 64000, "maxTokens": 4096}]
    }});
    let copilot_extra = serde_json::json!({
        "enterpriseUrl": "ghe.example.com", "availableModelIds": [kept_copilot_id]
    });
    write_credentials(
        &auth_path,
        &[
            (
                "radius",
                oauth_credential("radius-token", i64::MAX, radius_extra),
            ),
            (
                "github-copilot",
                oauth_credential("copilot-token", i64::MAX, copilot_extra),
            ),
        ],
    );

    let registry = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path)),
        models_path,
    );
    let radius = registry
        .find("radius", "radius-dynamic")
        .expect("Radius catalog model");
    assert_eq!(radius.api.as_ref(), "pi-messages");
    assert_eq!(radius.base_url, "https://radius.example/v1");
    let copilot = registry
        .get_all()
        .iter()
        .filter(|model| model.provider == "github-copilot")
        .collect::<Vec<_>>();
    assert_eq!(copilot.len(), 1);
    assert_eq!(copilot[0].id, copilot_ids[0]);
    assert_eq!(copilot[0].base_url, "https://copilot-api.ghe.example.com");
}

#[test]
fn custom_radius_oauth_registers_before_catalog_mutation() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let models_path = temp_dir.path().join("models.json");
    fs::write(
        &models_path,
        r#"{"providers":{"radius-corp":{
        "name":"Corporate Radius","baseUrl":"https://gateway.example/v1","oauth":"radius"
    }}}"#,
    )
    .unwrap();
    write_credentials(
        &auth_path,
        &[(
            "radius-corp",
            oauth_credential(
                "corp-token",
                i64::MAX,
                serde_json::json!({"gatewayConfig": {"baseUrl": "https://gateway.example/v1", "models": [{
                    "id": "corp-model", "name": "Corp Model", "reasoning": false, "input": ["text", "image"],
                    "cost": {"input": 0.1, "output": 0.2, "cacheRead": 0.0, "cacheWrite": 0.0},
                    "contextWindow": 32000, "maxTokens": 2048
                }]}}),
            ),
        )],
    );

    let registry = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path)),
        models_path,
    );
    assert!(pi_ai::oauth::get_oauth_login_provider("radius-corp").is_some());
    let model = registry
        .find("radius-corp", "corp-model")
        .expect("custom Radius catalog model");
    assert_eq!(model.base_url, "https://gateway.example/v1");
    assert_eq!(model.provider, "radius-corp");
}

#[tokio::test]
async fn oauth_refresh_failure_is_recorded_and_returns_unavailable() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let provider_id = "radius-refresh-soft-fail";
    let provider = std::sync::Arc::new(pi_ai::oauth::radius::create_radius_oauth_provider(
        pi_ai::oauth::radius::RadiusOAuthProviderOptions {
            id: provider_id.to_owned(),
            name: "Refresh failure".to_owned(),
            gateway: "http://127.0.0.1:9".to_owned(),
        },
    ));
    pi_ai::oauth::register_oauth_login_provider(provider_id, provider);
    write_credentials(
        &auth_path,
        &[(
            provider_id,
            oauth_credential("expired", 0, serde_json::json!({})),
        )],
    );

    let storage = AuthStorage::new(auth_path);
    assert_eq!(storage.get_api_key(provider_id, false).await.unwrap(), None);
    assert!(!storage.get_errors().is_empty());
    assert!(matches!(
        storage.get(provider_id).await.unwrap(),
        Some(Credential::OAuth(_))
    ));
}

#[tokio::test]
async fn oauth_refresh_failure_reloads_a_concurrently_refreshed_credential() {
    use std::io::{Read, Write};

    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let provider_id = "radius-refresh-race";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    let refreshed_auth_path = auth_path.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        write_credentials(
            &refreshed_auth_path,
            &[(
                provider_id,
                oauth_credential("fresh-access", i64::MAX, serde_json::json!({})),
            )],
        );
        stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let provider = std::sync::Arc::new(pi_ai::oauth::radius::create_radius_oauth_provider(
        pi_ai::oauth::radius::RadiusOAuthProviderOptions {
            id: provider_id.to_owned(),
            name: "Refresh race".to_owned(),
            gateway,
        },
    ));
    pi_ai::oauth::register_oauth_login_provider(provider_id, provider);
    write_credentials(
        &auth_path,
        &[(
            provider_id,
            oauth_credential("expired", 0, serde_json::json!({})),
        )],
    );

    let storage = AuthStorage::new(auth_path);
    assert_eq!(
        storage.get_api_key(provider_id, false).await.unwrap(),
        Some("fresh-access".to_owned())
    );
    assert!(!storage.get_errors().is_empty());
    server.join().unwrap();
}

#[test]
fn model_overrides_ignore_unsupported_transport_fields() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");
    let models_path = temp_dir.path().join("models.json");

    // Build baseline registry first to get the default model definition.
    let baseline_path = temp_dir.path().join("empty.json");
    let baseline = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path.clone())),
        baseline_path,
    );
    let original_model = baseline
        .find("openai", "gpt-4o")
        .expect("should find default gpt-4o")
        .clone();

    // Write a models.json containing unknown/unsupported fields in modelOverrides.
    // Also include a valid override field (like name) to prove it still applies.
    let config_content = r#"{
        "providers": {
            "openai": {
                "modelOverrides": {
                    "gpt-4o": {
                        "name": "GPT-4o Inert Override",
                        "api": "unsupported-api-val",
                        "baseUrl": "https://unsupported-base-url.example"
                    }
                }
            }
        }
    }"#;
    fs::write(&models_path, config_content).unwrap();

    let registry = ModelRegistry::create(
        std::sync::Arc::new(AuthStorage::new(auth_path)),
        models_path,
    );

    // Validation must pass, get_error() must be None
    assert!(
        registry.get_error().is_none(),
        "Expected no validation error, but got: {:?}",
        registry.get_error()
    );

    // Valid override (name) must be applied
    let overridden_model = registry
        .find("openai", "gpt-4o")
        .expect("should find gpt-4o");
    assert_eq!(overridden_model.name, "GPT-4o Inert Override");

    // Inert fields (api, baseUrl) must NOT alter the model's api or base_url
    assert_eq!(overridden_model.api, original_model.api);
    assert_eq!(overridden_model.base_url, original_model.base_url);
}
