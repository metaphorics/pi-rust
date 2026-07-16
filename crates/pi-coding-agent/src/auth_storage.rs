use std::collections::HashMap;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

use pi_ai::auth::{Credential, FileCredentialStore, CredentialStore};
use crate::resolve_config_value::resolve_config_value;

#[derive(Debug, thiserror::Error)]
pub enum AuthStorageError {
    #[error("credential storage JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("credential storage I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential storage error: {0}")]
    Store(#[from] pi_ai::auth::CredentialStoreError),
    #[error("resolve auth error: {0}")]
    Resolve(#[from] pi_ai::auth::ResolveAuthError),
    #[error("unresolved configuration value: {0}")]
    Unresolved(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

pub struct AuthStorage {
    store: FileCredentialStore,
    runtime_overrides: HashMap<String, String>,
}

impl AuthStorage {
    pub fn new(path: PathBuf) -> Self {
        Self {
            store: FileCredentialStore::new(path),
            runtime_overrides: HashMap::new(),
        }
    }

    /// Set a runtime API key override.
    pub fn set_runtime_api_key(&mut self, provider: String, api_key: String) {
        self.runtime_overrides.insert(provider, api_key);
    }

    /// Remove a runtime API key override.
    pub fn remove_runtime_api_key(&mut self, provider: &str) {
        self.runtime_overrides.remove(provider);
    }

    /// Check if credentials exist for a provider in auth.json.
    pub async fn has(&self, provider: &str) -> Result<bool, AuthStorageError> {
        let cred = self.store.read(provider).await?;
        Ok(cred.is_some())
    }

    /// Get raw credential for a provider.
    pub async fn get(&self, provider: &str) -> Result<Option<Credential>, AuthStorageError> {
        let cred = self.store.read(provider).await?;
        Ok(cred)
    }

    /// Set credential for a provider.
    pub async fn set(&self, provider: &str, credential: Credential) -> Result<(), AuthStorageError> {
        let cred = credential.clone();
        self.store.modify(provider, Box::new(move |_| Box::pin(async move { Ok(Some(cred)) }))).await?;
        Ok(())
    }

    /// Remove credential for a provider.
    pub async fn remove(&self, provider: &str) -> Result<(), AuthStorageError> {
        self.store.delete(provider).await?;
        Ok(())
    }

    /// Get provider-scoped environment values for an API key credential.
    pub async fn get_provider_env(&self, provider: &str) -> Result<Option<HashMap<String, String>>, AuthStorageError> {
        match self.store.read(provider).await? {
            Some(Credential::ApiKey(cred)) => Ok(cred.env),
            _ => Ok(None),
        }
    }

    /// Check if any form of auth is configured for a provider.
    pub async fn has_auth(&self, provider: &str) -> Result<bool, AuthStorageError> {
        if self.runtime_overrides.contains_key(provider) {
            return Ok(true);
        }
        if self.has(provider).await? {
            return Ok(true);
        }
        if pi_ai::env_api_keys::get_env_api_key(provider, None).is_some() {
            return Ok(true);
        }
        Ok(false)
    }

    /// Get auth status without exposing credential values or refreshing tokens.
    pub async fn get_auth_status(&self, provider: &str) -> Result<AuthStatus, AuthStorageError> {
        if self.has(provider).await? {
            return Ok(AuthStatus {
                configured: true,
                source: Some("stored".to_string()),
                label: None,
            });
        }

        if self.runtime_overrides.contains_key(provider) {
            return Ok(AuthStatus {
                configured: false,
                source: Some("runtime".to_string()),
                label: Some("--api-key".to_string()),
            });
        }

        if let Some(first_key) = pi_ai::env_api_keys::find_env_keys(provider, None).as_ref().and_then(|keys| keys.first()) {
            return Ok(AuthStatus {
                configured: false,
                source: Some("environment".to_string()),
                label: Some(first_key.to_string()),
            });
        }

        Ok(AuthStatus {
            configured: false,
            source: None,
            label: None,
        })
    }

    /// Get API key for a provider.
    pub async fn get_api_key(&self, provider_id: &str, include_fallback: bool) -> Result<Option<String>, AuthStorageError> {
        if let Some(key) = self.runtime_overrides.get(provider_id) {
            return Ok(Some(key.clone()));
        }

        let ambient_key = if include_fallback {
            pi_ai::env_api_keys::get_env_api_key(provider_id, None)
        } else {
            None
        };

        let oauth = pi_ai::oauth::get_oauth_provider(provider_id);
        let auth_res = match pi_ai::auth::resolve_provider_auth(
            &self.store,
            provider_id,
            oauth.as_deref(),
            ambient_key,
        ).await {
            Ok(Some(res)) => res,
            Ok(None) => return Ok(None),
            Err(e) => return Err(AuthStorageError::Resolve(e)),
        };

        if let Some(key) = auth_res.auth.api_key {
            Ok(resolve_config_value(&key, auth_res.env.as_ref()))
        } else {
            Ok(None)
        }
    }
}
