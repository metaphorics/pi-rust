//! Mocked end-to-end OAuth login flow tests (no live network).
//!
//! Each provider covers: auth URL shape, code exchange, token persist shape,
//! and refresh round-trip.

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use pi_ai::{
    auth::{CredentialStore, FileCredentialStore, InMemoryCredentialStore, OAuthAuth},
    oauth::{
        anthropic, github_copilot, openai_codex, radius,
        types::{
            OAuthAuthInfo, OAuthDeviceCodeInfo, OAuthLoginCallbacks, OAuthPrompt, OAuthProvider,
            OAuthSelectPrompt,
        },
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Handler = Arc<dyn Fn(String, String, String) -> (u16, &'static str, String) + Send + Sync>;

struct MockServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    _join: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn spawn(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = Arc::clone(&hits);
        let join = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                hits_task.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0_u8; 16384];
                let Ok(n) = stream.read(&mut buf).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let request_line = request.lines().next().unwrap_or("").to_owned();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_owned();
                let body = request.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
                let (status, content_type, response_body) = handler(request_line, path, body);
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            addr,
            hits,
            _join: join,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn jwt_with_account(account_id: &str) -> String {
    let payload = URL_SAFE_NO_PAD.encode(
        format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#)
            .as_bytes(),
    );
    format!("e30.{payload}.sig")
}

fn callbacks_with(
    auth_urls: Arc<Mutex<Vec<String>>>,
    device_codes: Arc<Mutex<Vec<OAuthDeviceCodeInfo>>>,
    select_id: Option<&'static str>,
    prompt_answers: Vec<String>,
) -> OAuthLoginCallbacks {
    let answers = Arc::new(Mutex::new(prompt_answers));
    OAuthLoginCallbacks {
        on_auth: Box::new(move |info: OAuthAuthInfo| {
            auth_urls.lock().unwrap().push(info.url);
        }),
        on_device_code: Box::new(move |info: OAuthDeviceCodeInfo| {
            device_codes.lock().unwrap().push(info);
        }),
        on_prompt: Box::new(move |_prompt: OAuthPrompt| {
            let answers = Arc::clone(&answers);
            Box::pin(async move {
                let mut guard = answers.lock().unwrap();
                if guard.is_empty() {
                    String::new()
                } else {
                    guard.remove(0)
                }
            }) as Pin<Box<dyn Future<Output = String> + Send>>
        }),
        on_progress: Some(Box::new(|_msg: &str| {})),
        on_manual_code_input: None,
        on_select: Box::new(move |_prompt: OAuthSelectPrompt| {
            let id = select_id.map(str::to_owned);
            Box::pin(async move { id }) as Pin<Box<dyn Future<Output = Option<String>> + Send>>
        }),
        cancellation: None,
        open_browser: false,
    }
}

#[tokio::test]
async fn anthropic_login_exchange_persist_and_refresh() {
    let auth_urls = Arc::new(Mutex::new(Vec::new()));
    let device_codes = Arc::new(Mutex::new(Vec::new()));

    let server = MockServer::spawn(Arc::new(|_line, _path, body| {
        if body.contains("grant_type\":\"authorization_code")
            || body.contains("\"grant_type\":\"authorization_code\"")
        {
            assert!(body.contains("\"code\":\"auth-code-1\""));
            assert!(body.contains("\"code_verifier\""));
            (
                200,
                "application/json",
                r#"{"access_token":"ant-access","refresh_token":"ant-refresh","expires_in":3600}"#
                    .into(),
            )
        } else if body.contains("refresh_token") {
            (
                200,
                "application/json",
                r#"{"access_token":"ant-access-2","refresh_token":"ant-refresh-2","expires_in":3600}"#
                    .into(),
            )
        } else {
            (400, "text/plain", "unexpected".into())
        }
    }))
    .await;

    // Capture verifier via authorize URL state (anthropic uses verifier as state).
    // We need the provider's generated PKCE — use with_test_callback after building URL
    // by first generating authorize URL shape test separately, then full login with injected code.
    let provider =
        anthropic::AnthropicOAuth::with_token_url(reqwest::Client::new(), server.url("/token"));

    // Pure URL shape
    let url = anthropic::build_authorize_url("challenge-abc", "state-xyz");
    assert!(url.starts_with(anthropic::AUTHORIZE_URL));
    assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
    assert!(url.contains("code_challenge=challenge-abc"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state=state-xyz"));
    assert!(url.contains("response_type=code"));

    // Injected callback (no real browser/port)
    let provider = provider.with_test_callback("auth-code-1", "state-for-exchange");
    let callbacks = callbacks_with(
        Arc::clone(&auth_urls),
        Arc::clone(&device_codes),
        None,
        vec![],
    );
    let credentials = provider.login(&callbacks).await.unwrap();
    assert_eq!(credentials.access, "ant-access");
    assert_eq!(credentials.refresh, "ant-refresh");
    assert!(credentials.expires > 0);
    assert!(!auth_urls.lock().unwrap().is_empty());
    let captured = auth_urls.lock().unwrap()[0].clone();
    assert!(captured.contains("code_challenge="));
    assert!(captured.contains("client_id="));

    // Persist via credential store
    let store = InMemoryCredentialStore::new();
    store
        .modify(
            "anthropic",
            Box::new({
                let credentials = credentials.clone();
                move |_| {
                    Box::pin(async move { Ok(Some(pi_ai::auth::Credential::OAuth(credentials))) })
                }
            }),
        )
        .await
        .unwrap();
    let loaded = store.read("anthropic").await.unwrap().unwrap();
    match loaded {
        pi_ai::auth::Credential::OAuth(o) => assert_eq!(o.access, "ant-access"),
        _ => panic!("expected oauth"),
    }

    // Refresh round-trip
    let refreshed = provider.refresh(&credentials).await.unwrap();
    assert_eq!(refreshed.access, "ant-access-2");
    assert_eq!(refreshed.refresh, "ant-refresh-2");
    assert!(server.hits() >= 2);
}

#[tokio::test]
async fn openai_codex_browser_login_and_device_code() {
    let jwt = jwt_with_account("acct_login");
    let token_json =
        format!(r#"{{"access_token":"{jwt}","refresh_token":"codex-refresh","expires_in":7200}}"#);
    let token_json2 = token_json.clone();

    let server = MockServer::spawn(Arc::new(move |_line, path, body| {
        if path.starts_with("/usercode") {
            (
                200,
                "application/json",
                r#"{"device_auth_id":"da1","user_code":"ABCD-EFGH","interval":1}"#.into(),
            )
        } else if path.starts_with("/device-token") {
            (
                200,
                "application/json",
                r#"{"authorization_code":"dev-code","code_verifier":"dev-verifier"}"#.into(),
            )
        } else if path.starts_with("/token") {
            if body.contains("authorization_code") || body.contains("grant_type=authorization_code")
            {
                (200, "application/json", token_json.clone())
            } else if body.contains("refresh_token") {
                let refresh_jwt = jwt_with_account("acct_refresh");
                (
                    200,
                    "application/json",
                    format!(
                        r#"{{"access_token":"{refresh_jwt}","refresh_token":"codex-refresh-2","expires_in":7200}}"#
                    ),
                )
            } else {
                (200, "application/json", token_json.clone())
            }
        } else {
            (404, "text/plain", "no".into())
        }
    }))
    .await;

    // Browser path with injected code
    let auth_urls = Arc::new(Mutex::new(Vec::new()));
    let device_codes = Arc::new(Mutex::new(Vec::new()));
    let provider = openai_codex::OpenAICodexOAuth::with_endpoints(
        reqwest::Client::new(),
        server.url("/token"),
        server.url("/usercode"),
        server.url("/device-token"),
    )
    .with_test_callback("browser-code");

    let flow = openai_codex::create_authorization_flow("pi");
    assert!(flow.url.contains("response_type=code"));
    assert!(flow.url.contains("code_challenge_method=S256"));
    assert!(flow.url.contains("originator=pi"));
    assert!(flow.url.contains("codex_cli_simplified_flow=true"));
    assert!(!flow.verifier.is_empty());

    let callbacks = callbacks_with(
        Arc::clone(&auth_urls),
        Arc::clone(&device_codes),
        Some(openai_codex::OPENAI_CODEX_BROWSER_LOGIN_METHOD),
        vec![],
    );
    let credentials = provider.login(&callbacks).await.unwrap();
    assert_eq!(credentials.extra["accountId"], "acct_login");
    assert!(!auth_urls.lock().unwrap().is_empty());

    // Device-code path
    let auth_urls2 = Arc::new(Mutex::new(Vec::new()));
    let device_codes2 = Arc::new(Mutex::new(Vec::new()));
    let provider2 = openai_codex::OpenAICodexOAuth::with_endpoints(
        reqwest::Client::new(),
        server.url("/token"),
        server.url("/usercode"),
        server.url("/device-token"),
    );
    let callbacks2 = callbacks_with(
        auth_urls2,
        Arc::clone(&device_codes2),
        Some(openai_codex::OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD),
        vec![],
    );
    let device_creds = provider2.login(&callbacks2).await.unwrap();
    assert_eq!(device_creds.extra["accountId"], "acct_login");
    let dc = device_codes2.lock().unwrap();
    assert_eq!(dc.len(), 1);
    assert_eq!(dc[0].user_code, "ABCD-EFGH");
    assert_eq!(
        dc[0].verification_uri,
        openai_codex::DEVICE_VERIFICATION_URI
    );

    // Refresh
    let _ = token_json2;
    let refreshed = provider.refresh(&credentials).await.unwrap();
    assert_eq!(refreshed.extra["accountId"], "acct_refresh");
}

#[tokio::test]
async fn github_copilot_device_login_exchange_and_refresh() {
    let server = MockServer::spawn(Arc::new(|_line, path, body| {
        if path.starts_with("/device/code") {
            (
                200,
                "application/json",
                r#"{"device_code":"dc1","user_code":"XXXX-YYYY","verification_uri":"https://github.com/login/device","interval":1,"expires_in":900}"#.into(),
            )
        } else if path.starts_with("/oauth/access_token") {
            if body.contains("device_code") {
                (
                    200,
                    "application/json",
                    r#"{"access_token":"gho_github_pat"}"#.into(),
                )
            } else {
                (400, "text/plain", "bad".into())
            }
        } else if path.starts_with("/copilot_internal/v2/token") {
            (
                200,
                "application/json",
                r#"{"token":"tid=x;proxy-ep=proxy.individual.githubcopilot.com;exp=1","expires_at":2000000000}"#.into(),
            )
        } else if path.starts_with("/models") {
            (
                200,
                "application/json",
                r#"{"data":[{"id":"gpt-4.1","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}}]}"#.into(),
            )
        } else {
            (404, "text/plain", "missing".into())
        }
    }))
    .await;

    let provider = github_copilot::GitHubCopilotOAuth::with_login_endpoints(
        reqwest::Client::new(),
        server.url("/device/code"),
        server.url("/oauth/access_token"),
        server.url("/copilot_internal/v2/token"),
        server.url("/models"),
    );

    let auth_urls = Arc::new(Mutex::new(Vec::new()));
    let device_codes = Arc::new(Mutex::new(Vec::new()));
    let callbacks = callbacks_with(
        auth_urls,
        Arc::clone(&device_codes),
        None,
        vec!["".into()], // blank enterprise domain
    );
    let credentials = provider.login(&callbacks).await.unwrap();
    assert!(
        credentials
            .access
            .contains("proxy-ep=proxy.individual.githubcopilot.com")
    );
    assert_eq!(credentials.refresh, "gho_github_pat");
    assert_eq!(
        credentials.extra["availableModelIds"],
        serde_json::json!(["gpt-4.1"])
    );
    let dc = device_codes.lock().unwrap();
    assert_eq!(dc[0].user_code, "XXXX-YYYY");

    let auth = github_copilot::to_auth(&credentials);
    assert_eq!(
        auth.base_url.as_deref(),
        Some("https://api.individual.githubcopilot.com")
    );

    // Dynamic headers
    let headers = github_copilot::build_copilot_dynamic_headers(&[], false);
    assert_eq!(headers.get("X-Initiator").map(String::as_str), Some("user"));
    assert_eq!(
        headers.get("Openai-Intent").map(String::as_str),
        Some("conversation-edits")
    );

    let refreshed = provider.refresh(&credentials).await.unwrap();
    assert!(refreshed.access.contains("proxy-ep="));
}

#[tokio::test]
async fn radius_factory_registered_and_browser_login() {
    assert!(pi_ai::oauth::get_oauth_provider("radius").is_some());
    assert!(pi_ai::oauth::get_oauth_login_provider("radius").is_some());

    let server = MockServer::spawn(Arc::new(|_line, path, body| {
        if path.starts_with("/v1/oauth") {
            (
                200,
                "application/json",
                format!(
                    r#"{{
                      "issuer":"http://radius.test",
                      "authorizationEndpoint":"http://radius.test/authorize",
                      "tokenEndpoint":"PLACEHOLDER_TOKEN",
                      "deviceAuthorizationEndpoint":"http://radius.test/device",
                      "verificationEndpoint":"http://radius.test/verify",
                      "clientId":"radius-client",
                      "scope":"openid",
                      "deviceCodeGrantType":"urn:ietf:params:oauth:grant-type:device_code"
                    }}"#
                )
                .replace("PLACEHOLDER_TOKEN", "WILL_PATCH"),
            )
        } else if path.starts_with("/v1/config") {
            (
                200,
                "application/json",
                r#"{"baseUrl":"https://radius.example/v1","models":[{"id":"m1","name":"M1","reasoning":false,"input":["text"],"cost":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"contextWindow":1000,"maxTokens":100}]}"#.into(),
            )
        } else if path.starts_with("/token") {
            if body.contains("authorization_code") {
                (
                    200,
                    "application/json",
                    r#"{"access_token":"rad-access","refresh_token":"rad-refresh","expires_in":3600,"scope":"openid"}"#.into(),
                )
            } else {
                (
                    200,
                    "application/json",
                    r#"{"access_token":"rad-access-2","refresh_token":"rad-refresh-2","expires_in":3600}"#.into(),
                )
            }
        } else {
            (404, "text/plain", "no".into())
        }
    }))
    .await;

    // Re-spawn with correct token endpoint embedded (handler closes over server addr).
    let token_url = server.url("/token");
    let _gateway = format!("http://{}", server.addr);
    let server = MockServer::spawn(Arc::new({
        let token_url = token_url.clone();
        move |_line, path, body| {
            if path.starts_with("/v1/oauth") {
                (
                    200,
                    "application/json",
                    format!(
                        r#"{{
                      "issuer":"http://radius.test",
                      "authorizationEndpoint":"http://radius.test/authorize",
                      "tokenEndpoint":"{token_url}",
                      "deviceAuthorizationEndpoint":"http://radius.test/device",
                      "verificationEndpoint":"http://radius.test/verify",
                      "clientId":"radius-client",
                      "scope":"openid",
                      "deviceCodeGrantType":"urn:ietf:params:oauth:grant-type:device_code"
                    }}"#
                    ),
                )
            } else if path.starts_with("/v1/config") {
                (
                    200,
                    "application/json",
                    r#"{"baseUrl":"https://radius.example/v1","models":[{"id":"m1","name":"M1","reasoning":false,"input":["text"],"cost":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"contextWindow":1000,"maxTokens":100}]}"#.into(),
                )
            } else if path.starts_with("/token") {
                if body.contains("grant_type=authorization_code")
                    || body.contains("authorization_code")
                {
                    (
                        200,
                        "application/json",
                        r#"{"access_token":"rad-access","refresh_token":"rad-refresh","expires_in":3600,"scope":"openid"}"#.into(),
                    )
                } else {
                    (
                        200,
                        "application/json",
                        r#"{"access_token":"rad-access-2","refresh_token":"rad-refresh-2","expires_in":3600}"#.into(),
                    )
                }
            } else {
                (404, "text/plain", "no".into())
            }
        }
    }))
    .await;
    let gateway = format!("http://{}", server.addr);

    let provider = radius::RadiusOAuth::create_with_client(
        reqwest::Client::new(),
        radius::RadiusOAuthProviderOptions {
            id: "radius".into(),
            name: "Radius".into(),
            gateway: gateway.clone(),
        },
    )
    .with_test_callback("radius-code");

    let auth_urls = Arc::new(Mutex::new(Vec::new()));
    let device_codes = Arc::new(Mutex::new(Vec::new()));
    let callbacks = callbacks_with(
        Arc::clone(&auth_urls),
        device_codes,
        Some(radius::LOGIN_METHOD_BROWSER),
        vec![],
    );
    let credentials = provider.login(&callbacks).await.unwrap();
    assert_eq!(credentials.access, "rad-access");
    assert_eq!(credentials.refresh, "rad-refresh");
    assert!(credentials.extra.get("gatewayConfig").is_some());
    assert!(!auth_urls.lock().unwrap().is_empty());
    let url = &auth_urls.lock().unwrap()[0];
    assert!(url.contains("code_challenge="));
    assert!(url.contains("client_id=radius-client"));

    // File persist round-trip (byte-compatible shape)
    let dir = tempfile_dir();
    let path = dir.join("auth.json");
    let store = FileCredentialStore::new(&path);
    store
        .modify(
            "radius",
            Box::new({
                let credentials = credentials.clone();
                move |_| {
                    Box::pin(async move { Ok(Some(pi_ai::auth::Credential::OAuth(credentials))) })
                }
            }),
        )
        .await
        .unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("\"type\": \"oauth\"") || raw.contains("\"type\":\"oauth\""));
    assert!(raw.contains("rad-access"));

    let refreshed = provider.refresh(&credentials).await.unwrap();
    assert_eq!(refreshed.access, "rad-access-2");
}

fn tempfile_dir() -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "pi-ai-oauth-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn oauth_page_html_is_dark_theme_with_logo() {
    let html = pi_ai::oauth::oauth_page::oauth_success_html("ok");
    assert!(html.contains("--page-bg: #09090b"));
    assert!(html.contains("viewBox=\"0 0 800 800\""));
    assert!(html.contains("Authentication successful"));
}

#[test]
fn parse_authorization_input_handles_url_and_raw() {
    let (code, state) = pi_ai::oauth::types::parse_authorization_input(
        "http://localhost:53692/callback?code=abc&state=xyz",
    );
    assert_eq!(code.as_deref(), Some("abc"));
    assert_eq!(state.as_deref(), Some("xyz"));
    let (code, _) = pi_ai::oauth::types::parse_authorization_input("plain-code");
    assert_eq!(code.as_deref(), Some("plain-code"));
}
