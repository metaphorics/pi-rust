use std::{collections::HashMap, path::PathBuf};

use pi_ai::auth::{Credential, CredentialStore, FileCredentialStore};
use serde_json::Value;

fn temp_auth_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "pi-ai-auth-{}-{}.json",
        std::process::id(),
        jiff::Timestamp::now().as_nanosecond()
    ))
}

#[tokio::test]
async fn file_store_roundtrips_auth_json_and_enforces_private_mode() {
    let path = temp_auth_path();
    tokio::fs::write(&path, include_str!("fixtures/auth/sample_auth.json"))
        .await
        .unwrap();
    let store = FileCredentialStore::new(&path);

    let anthropic = store.read("anthropic").await.unwrap().unwrap();
    assert!(matches!(anthropic, Credential::OAuth(_)));

    store
        .modify(
            "openai",
            Box::new(|current| Box::pin(async move { Ok(current) })),
        )
        .await
        .unwrap();

    let actual: Value = serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/auth/sample_auth.json")).unwrap();
    assert_eq!(actual, expected);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    tokio::fs::remove_file(path).await.unwrap();
}

#[test]
fn auth_fixture_preserves_extension_fields() {
    let auth: HashMap<String, Credential> =
        serde_json::from_str(include_str!("fixtures/auth/sample_auth.json")).unwrap();
    let Credential::OAuth(oauth) = &auth["anthropic"] else {
        panic!("expected oauth")
    };
    assert_eq!(oauth.extra["scope"], "user:inference");
}
