use super::{
    helpers::{api_key_credential_auth, env_api_key_auth},
    types::{AuthResult, Credential, CredentialStore, CredentialStoreError, OAuthAuth, OAuthError},
};

#[derive(Debug, thiserror::Error)]
pub enum ResolveAuthError {
    #[error(transparent)]
    Store(#[from] CredentialStoreError),
    #[error(transparent)]
    OAuth(#[from] OAuthError),
}

/// Resolves stored credentials, refreshing expired OAuth credentials while the
/// credential store's serialized modify lock is held.
pub async fn resolve_provider_auth(
    store: &dyn CredentialStore,
    provider_id: &str,
    oauth: Option<&dyn OAuthAuth>,
    ambient_api_key: Option<String>,
) -> Result<Option<AuthResult>, ResolveAuthError> {
    let credential = if let Some(oauth) = oauth {
        store
            .modify(
                provider_id,
                Box::new(move |current| {
                    Box::pin(async move {
                        let Some(Credential::OAuth(credential)) = current else {
                            return Ok(None);
                        };
                        let now_ms = jiff::Timestamp::now().as_millisecond();
                        if credential.expires <= now_ms {
                            let refreshed = oauth
                                .refresh(&credential)
                                .await
                                .map_err(|error| CredentialStoreError::Update(error.to_string()))?;
                            Ok(Some(Credential::OAuth(refreshed)))
                        } else {
                            Ok(None)
                        }
                    })
                }),
            )
            .await?
    } else {
        store.read(provider_id).await?
    };

    match credential {
        Some(Credential::ApiKey(credential)) => Ok(api_key_credential_auth(&credential)),
        Some(Credential::OAuth(credential)) => {
            let Some(oauth) = oauth else {
                return Ok(None);
            };
            Ok(Some(AuthResult {
                auth: oauth.to_auth(&credential).await?,
                env: None,
                source: Some("OAuth".into()),
            }))
        }
        None => Ok(ambient_api_key.map(|key| AuthResult {
            auth: env_api_key_auth(key),
            env: None,
            source: Some("Environment".into()),
        })),
    }
}
