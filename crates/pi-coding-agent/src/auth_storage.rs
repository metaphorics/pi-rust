use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use crate::resolve_config_value::resolve_config_value;
use pi_ai::auth::{
    Credential, CredentialStore, CredentialStoreError, FileCredentialStore, ResolveAuthError,
};

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
    path: PathBuf,
    runtime_overrides: RwLock<HashMap<String, String>>,
    errors: Mutex<Vec<String>>,
}

impl AuthStorage {
    pub fn new(path: PathBuf) -> Self {
        Self {
            store: FileCredentialStore::new(path.clone()),
            path,
            runtime_overrides: RwLock::new(HashMap::new()),
            errors: Mutex::new(Vec::new()),
        }
    }

    /// Set a runtime API key override.
    pub fn set_runtime_api_key(&self, provider: String, api_key: String) {
        self.runtime_overrides
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(provider, api_key);
    }

    /// Remove a runtime API key override.
    pub fn remove_runtime_api_key(&self, provider: &str) {
        self.runtime_overrides
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(provider);
    }

    fn runtime_override(&self, provider: &str) -> Option<String> {
        self.runtime_overrides
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(provider)
            .cloned()
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
    pub async fn set(
        &self,
        provider: &str,
        credential: Credential,
    ) -> Result<(), AuthStorageError> {
        let cred = credential.clone();
        self.store
            .modify(
                provider,
                Box::new(move |_| Box::pin(async move { Ok(Some(cred)) })),
            )
            .await?;
        Ok(())
    }

    /// Remove credential for a provider.
    pub async fn remove(&self, provider: &str) -> Result<(), AuthStorageError> {
        self.store.delete(provider).await?;
        Ok(())
    }

    /// Get provider-scoped environment values for an API key credential.
    pub async fn get_provider_env(
        &self,
        provider: &str,
    ) -> Result<Option<HashMap<String, String>>, AuthStorageError> {
        match self.store.read(provider).await? {
            Some(Credential::ApiKey(cred)) => Ok(cred.env),
            _ => Ok(None),
        }
    }

    /// Check if any form of auth is configured for a provider.
    pub async fn has_auth(&self, provider: &str) -> Result<bool, AuthStorageError> {
        if self.runtime_override(provider).is_some() {
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

        if self.runtime_override(provider).is_some() {
            return Ok(AuthStatus {
                configured: false,
                source: Some("runtime".to_string()),
                label: Some("--api-key".to_string()),
            });
        }

        if let Some(first_key) = pi_ai::env_api_keys::find_env_keys(provider, None)
            .as_ref()
            .and_then(|keys| keys.first())
        {
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

    /// Read a raw credential directly from auth.json for synchronous catalog loading.
    pub(crate) fn get_sync(&self, provider: &str) -> Result<Option<Credential>, AuthStorageError> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let credentials: HashMap<String, Credential> = serde_json::from_str(&content)?;
        Ok(credentials.get(provider).cloned())
    }

    pub fn get_errors(&self) -> Vec<String> {
        self.errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record_error(&self, error: &impl std::fmt::Display) {
        self.errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(error.to_string());
    }

    /// Get API key for a provider.
    pub async fn get_api_key(
        &self,
        provider_id: &str,
        include_fallback: bool,
    ) -> Result<Option<String>, AuthStorageError> {
        if let Some(key) = self.runtime_override(provider_id) {
            return Ok(Some(key));
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
        )
        .await
        {
            Ok(Some(res)) => res,
            Ok(None) => return Ok(None),
            Err(ResolveAuthError::Store(CredentialStoreError::Update(message))) => {
                self.record_error(&message);
                return self
                    .recover_after_oauth_refresh_failure(provider_id, oauth.as_deref())
                    .await;
            }
            Err(ResolveAuthError::OAuth(error)) => {
                self.record_error(&error);
                return self
                    .recover_after_oauth_refresh_failure(provider_id, oauth.as_deref())
                    .await;
            }
            Err(error) => return Err(AuthStorageError::Resolve(error)),
        };

        if let Some(key) = auth_res.auth.api_key {
            Ok(resolve_config_value(&key, auth_res.env.as_ref()))
        } else {
            Ok(None)
        }
    }

    async fn recover_after_oauth_refresh_failure(
        &self,
        provider_id: &str,
        oauth: Option<&dyn pi_ai::auth::OAuthAuth>,
    ) -> Result<Option<String>, AuthStorageError> {
        let Some(Credential::OAuth(credential)) = self.store.read(provider_id).await? else {
            return Ok(None);
        };
        if credential.expires <= jiff::Timestamp::now().as_millisecond() {
            return Ok(None);
        }
        let Some(oauth) = oauth else {
            return Ok(None);
        };
        match oauth.to_auth(&credential).await {
            Ok(auth) => Ok(auth.api_key),
            Err(error) => {
                self.record_error(&error);
                Ok(None)
            }
        }
    }
}
