use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
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
    assert_eq!(
        anthropic::to_auth(&credential("anthropic-access"))
            .api_key
            .as_deref(),
        Some("anthropic-access")
    );
    assert_eq!(
        openai_codex::to_auth(&credential("codex-access"))
            .api_key
            .as_deref(),
        Some("codex-access")
    );
}

#[test]
fn copilot_derives_individual_proxy_and_enterprise_urls() {
    let proxy = credential("tid=x;proxy-ep=proxy.business.githubcopilot.com;exp=1");
    assert_eq!(
        github_copilot::to_auth(&proxy).base_url.as_deref(),
        Some("https://api.business.githubcopilot.com")
    );

    let mut enterprise = credential("opaque");
    enterprise.extra.insert(
        "enterpriseUrl".into(),
        Value::String("https://company.ghe.com/path".into()),
    );
    assert_eq!(
        github_copilot::to_auth(&enterprise).base_url.as_deref(),
        Some("https://copilot-api.company.ghe.com")
    );
    assert_eq!(
        github_copilot::get_base_url(None, None),
        "https://api.individual.githubcopilot.com"
    );
    assert_eq!(
        github_copilot::get_base_url(Some("proxy-ep=api.proxy.githubcopilot.com;"), None),
        "https://api.proxy.githubcopilot.com"
    );
}

#[test]
fn codex_refresh_credentials_preserve_account_id_from_jwt() {
    let payload = URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_123"}}"#);
    let token = format!("e30.{payload}.signature");
    let credential = openai_codex::credentials_from_token(token, "refresh".into(), 123).unwrap();
    assert_eq!(credential.extra["accountId"], "acct_123");
    assert_eq!(
        openai_codex::to_auth(&credential).headers.unwrap()["chatgpt-account-id"].as_deref(),
        Some("acct_123")
    );
}

#[test]
fn copilot_model_policy_filter_matches_pi() {
    let response = serde_json::json!({"data": [
        {"id":"enabled","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}},
        {"id":"disabled","model_picker_enabled":true,"policy":{"state":"disabled"},"capabilities":{"supports":{"tool_calls":true}}},
        {"id":"no-tools","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":false}}},
        {"id":"hidden","model_picker_enabled":false,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}}
    ]});
    assert_eq!(
        github_copilot::parse_available_model_ids(&response).unwrap(),
        vec!["enabled"]
    );
}
