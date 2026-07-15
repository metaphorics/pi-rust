use super::types::{ApiKeyCredential, AuthResult, ModelAuth};

pub fn env_api_key_auth(key: impl Into<String>) -> ModelAuth {
    ModelAuth {
        api_key: Some(key.into()),
        headers: None,
        base_url: None,
    }
}

pub fn api_key_credential_auth(credential: &ApiKeyCredential) -> Option<AuthResult> {
    credential.key.as_ref().map(|key| AuthResult {
        auth: env_api_key_auth(key.clone()),
        env: credential.env.clone(),
        source: Some("Stored API key".into()),
    })
}
