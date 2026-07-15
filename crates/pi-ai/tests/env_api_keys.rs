use std::collections::HashMap;

use pi_ai::env_api_keys::{find_env_keys, get_api_key_env_vars, get_env_api_key};

fn env(values: &[(&str, &str)]) -> HashMap<String, String> {
    values.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect()
}

#[test]
fn key_precedence_and_provider_map_match_pi() {
    let values = env(&[("ANTHROPIC_API_KEY", "api"), ("ANTHROPIC_OAUTH_TOKEN", "oauth")]);
    assert_eq!(get_api_key_env_vars("anthropic").unwrap(), &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]);
    assert_eq!(find_env_keys("anthropic", Some(&values)).unwrap(), vec!["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]);
    assert_eq!(get_env_api_key("anthropic", Some(&values)).as_deref(), Some("oauth"));
}

#[test]
fn bedrock_reports_ambient_authentication() {
    let values = env(&[("AWS_ACCESS_KEY_ID", "id"), ("AWS_SECRET_ACCESS_KEY", "secret")]);
    assert_eq!(get_env_api_key("amazon-bedrock", Some(&values)).as_deref(), Some("<authenticated>"));
}

#[test]
fn vertex_requires_adc_project_and_location() {
    let path = std::env::temp_dir().join(format!("pi-ai-adc-{}", std::process::id()));
    std::fs::write(&path, "{}").unwrap();
    let values = env(&[
        ("GOOGLE_APPLICATION_CREDENTIALS", path.to_str().unwrap()),
        ("GOOGLE_CLOUD_PROJECT", "project"),
        ("GOOGLE_CLOUD_LOCATION", "us-central1"),
    ]);
    assert_eq!(get_env_api_key("google-vertex", Some(&values)).as_deref(), Some("<authenticated>"));
    std::fs::remove_file(path).unwrap();
}
