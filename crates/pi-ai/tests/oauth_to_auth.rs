use std::collections::HashMap;

use pi_ai::{
    auth::OAuthCredential,
    oauth::{anthropic, github_copilot, openai_codex},
};
use serde_json::Value;

fn credential(access: &str) -> OAuthCredential {
    OAuthCredential {
        access: access.into(),
        refresh: "refresh".into(),
        expires: 2_000_000_000_000,
        extra: HashMap::new(),
    }
}

#[test]
fn oauth_client_ids_match_pi() {
    assert_eq!(anthropic::CLIENT_ID, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(github_copilot::CLIENT_ID, "Iv1.b507a08c87ecfe98");
    assert_eq!(openai_codex::CLIENT_ID, "app_EMoamEEZ73f0CkXaXp7hrann");
}

#[test]
fn anthropic_and_codex_use_access_token_as_api_key() {
    assert_eq!(anthropic::to_auth(&credential("anthropic-access")).api_key.as_deref(), Some("anthropic-access"));
    assert_eq!(openai_codex::to_auth(&credential("codex-access")).api_key.as_deref(), Some("codex-access"));
}

#[test]
fn copilot_derives_individual_proxy_and_enterprise_urls() {
    let proxy = credential("tid=x;proxy-ep=proxy.business.githubcopilot.com;exp=1");
    assert_eq!(
        github_copilot::to_auth(&proxy).base_url.as_deref(),
        Some("https://api.business.githubcopilot.com")
    );

    let mut enterprise = credential("opaque");
    enterprise.extra.insert("enterpriseUrl".into(), Value::String("https://company.ghe.com/path".into()));
    assert_eq!(
        github_copilot::to_auth(&enterprise).base_url.as_deref(),
        Some("https://copilot-api.company.ghe.com")
    );
    assert_eq!(github_copilot::get_base_url(None, None), "https://api.individual.githubcopilot.com");
}
