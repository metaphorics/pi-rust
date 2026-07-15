use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::types::{Credential, CredentialModifier, CredentialStore, CredentialStoreError};

#[derive(Default)]
pub struct InMemoryCredentialStore {
    credentials: Mutex<HashMap<String, Credential>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(&self, provider_id: &str) -> Result<Option<Credential>, CredentialStoreError> {
        Ok(self.credentials.lock().await.get(provider_id).cloned())
    }

    async fn modify<'a>(
        &'a self,
        provider_id: &str,
        modifier: CredentialModifier<'a>,
    ) -> Result<Option<Credential>, CredentialStoreError> {
        let mut credentials = self.credentials.lock().await;
        let current = credentials.get(provider_id).cloned();
        let next = modifier(current.clone()).await?;
        if let Some(next) = next {
            credentials.insert(provider_id.to_owned(), next.clone());
            Ok(Some(next))
        } else {
            Ok(current)
        }
    }

    async fn delete(&self, provider_id: &str) -> Result<(), CredentialStoreError> {
        self.credentials.lock().await.remove(provider_id);
        Ok(())
    }
}

pub struct FileCredentialStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileCredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn read_all(&self) -> Result<HashMap<String, Credential>, CredentialStoreError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_all(
        &self,
        credentials: &HashMap<String, Credential>,
    ) -> Result<(), CredentialStoreError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut json = serde_json::to_string_pretty(credentials)?;
        json.push('\n');

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use tokio::io::AsyncWriteExt;

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&self.path)
                .await?;
            file.write_all(json.as_bytes()).await?;
            file.flush().await?;
            tokio::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        #[cfg(not(unix))]
        tokio::fs::write(&self.path, json).await?;

        Ok(())
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn read(&self, provider_id: &str) -> Result<Option<Credential>, CredentialStoreError> {
        let _guard = self.lock.lock().await;
        Ok(self.read_all().await?.get(provider_id).cloned())
    }

    async fn modify<'a>(
        &'a self,
        provider_id: &str,
        modifier: CredentialModifier<'a>,
    ) -> Result<Option<Credential>, CredentialStoreError> {
        let _guard = self.lock.lock().await;
        let mut credentials = self.read_all().await?;
        let current = credentials.get(provider_id).cloned();
        let next = modifier(current.clone()).await?;
        if let Some(next) = next {
            credentials.insert(provider_id.to_owned(), next.clone());
            self.write_all(&credentials).await?;
            Ok(Some(next))
        } else {
            Ok(current)
        }
    }

    async fn delete(&self, provider_id: &str) -> Result<(), CredentialStoreError> {
        let _guard = self.lock.lock().await;
        let mut credentials = self.read_all().await?;
        if credentials.remove(provider_id).is_some() {
            self.write_all(&credentials).await?;
        }
        Ok(())
    }
}
