//! Hand-rolled AWS Signature Version 4 for Bedrock HTTP requests.
//!
//! Footprint: hmac + sha2 (already a dep) only — no aws-sigv4/smithy tree.
//! Credential chain is env (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
//! `AWS_SESSION_TOKEN` / `AWS_REGION` / `AWS_DEFAULT_REGION` / `AWS_PROFILE`) then
//! `~/.aws/credentials` + `~/.aws/config` profile files. Bearer-token auth
//! bypasses this module entirely (oracle bedrock-converse-stream.ts:93-98).
//!
//! Verified against AWS-published SigV4 GET test vectors (not live network).

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Static credentials for SigV4 (env or profile).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Resolve region + credentials the way the Bedrock oracle does (env first, then profile).
pub fn resolve_region(options_env: Option<&std::collections::HashMap<String, String>>) -> String {
    env_lookup("AWS_REGION", options_env)
        .or_else(|| env_lookup("AWS_DEFAULT_REGION", options_env))
        .or_else(|| profile_region(options_env))
        .unwrap_or_else(|| "us-east-1".into())
}

pub fn resolve_credentials(
    options_env: Option<&std::collections::HashMap<String, String>>,
) -> Option<AwsCredentials> {
    if let (Some(access_key_id), Some(secret_access_key)) = (
        env_lookup("AWS_ACCESS_KEY_ID", options_env),
        env_lookup("AWS_SECRET_ACCESS_KEY", options_env),
    ) {
        return Some(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: env_lookup("AWS_SESSION_TOKEN", options_env),
        });
    }
    load_profile_credentials(options_env)
}

fn env_lookup(
    key: &str,
    options_env: Option<&std::collections::HashMap<String, String>>,
) -> Option<String> {
    options_env
        .and_then(|env| env.get(key).cloned())
        .or_else(|| env::var(key).ok())
        .filter(|value| !value.is_empty())
}

fn profile_name(options_env: Option<&std::collections::HashMap<String, String>>) -> String {
    env_lookup("AWS_PROFILE", options_env).unwrap_or_else(|| "default".into())
}

fn aws_home() -> PathBuf {
    if let Ok(dir) = env::var("AWS_SHARED_CREDENTIALS_FILE")
        && let Some(parent) = Path::new(&dir).parent()
    {
        return parent.to_path_buf();
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".aws");
    }
    PathBuf::from(".aws")
}

fn load_profile_credentials(
    options_env: Option<&std::collections::HashMap<String, String>>,
) -> Option<AwsCredentials> {
    let profile = profile_name(options_env);
    let credentials_path = env::var("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| aws_home().join("credentials"));
    let map = parse_ini(&credentials_path).ok()?;
    let section = map.get(&profile).or_else(|| map.get("default"))?;
    let access_key_id = section.get("aws_access_key_id")?.clone();
    let secret_access_key = section.get("aws_secret_access_key")?.clone();
    let session_token = section.get("aws_session_token").cloned();
    Some(AwsCredentials {
        access_key_id,
        secret_access_key,
        session_token,
    })
}

fn profile_region(
    options_env: Option<&std::collections::HashMap<String, String>>,
) -> Option<String> {
    let profile = profile_name(options_env);
    let config_path = env::var("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| aws_home().join("config"));
    let map = parse_ini(&config_path).ok()?;
    let section_name = if profile == "default" {
        "default".to_owned()
    } else {
        format!("profile {profile}")
    };
    map.get(&section_name)
        .or_else(|| map.get(&profile))
        .and_then(|section| section.get("region").cloned())
}

fn parse_ini(path: &Path) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current = String::from("default");
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = name.trim().to_owned();
            out.entry(current.clone()).or_default();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.entry(current.clone())
                .or_default()
                .insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    Ok(out)
}

/// Sign a POST request. Returns headers to merge onto the outgoing request
/// (`authorization`, `x-amz-date`, optional `x-amz-security-token`, `x-amz-content-sha256`).
pub fn sign_post_headers(
    url: &str,
    body: &[u8],
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    amz_date: &str,
) -> Result<Vec<(String, String)>, String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("invalid URL for SigV4: {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "SigV4 URL missing host".to_owned())?
        .to_owned();
    let path = if parsed.path().is_empty() {
        "/".to_owned()
    } else {
        canonical_uri_path(parsed.path())
    };
    let query = canonical_query(parsed.query().unwrap_or(""));
    let payload_hash = hex_sha256(body);
    let date_stamp = amz_date
        .get(..8)
        .ok_or_else(|| "amz_date must be YYYYMMDD'T'HHMMSS'Z'".to_owned())?;

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert("content-type".into(), "application/json".into());
    headers.insert("host".into(), host);
    headers.insert("x-amz-content-sha256".into(), payload_hash.clone());
    headers.insert("x-amz-date".into(), amz_date.to_owned());
    if let Some(token) = &credentials.session_token {
        headers.insert("x-amz-security-token".into(), token.clone());
    }

    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", trim_all(v)))
        .collect::<String>();
    let canonical_request =
        format!("POST\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );
    let signing_key =
        derive_signing_key(&credentials.secret_access_key, date_stamp, region, service);
    let signature = hex_hmac(&signing_key, string_to_sign.as_bytes());
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        credentials.access_key_id, credential_scope, signed_headers, signature
    );

    let mut out = vec![
        ("authorization".into(), authorization),
        ("x-amz-date".into(), amz_date.to_owned()),
        ("x-amz-content-sha256".into(), payload_hash),
        ("content-type".into(), "application/json".into()),
    ];
    if let Some(token) = &credentials.session_token {
        out.push(("x-amz-security-token".into(), token.clone()));
    }
    Ok(out)
}

pub fn amz_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Format as YYYYMMDDTHHMMSSZ without pulling chrono.
    let days = secs / 86_400;
    let day_secs = secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Days since 1970-01-01 → (year, month, day). Algorithm from Howard Hinnant.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn canonical_uri_path(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.split('/')
        .map(|segment| uri_encode(&percent_decode(segment), true))
        .collect::<Vec<_>>()
        .join("/")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (uri_encode(k, true), uri_encode(v, true)),
            None => (uri_encode(part, true), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

fn trim_all(value: &str) -> String {
    let collapsed: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn derive_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_hmac(key: &[u8], data: &[u8]) -> String {
    hex_encode(&hmac_sha256(key, data))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS SigV4 docs published example credentials (NOT live secrets).
    /// Source: https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html
    const EXAMPLE_ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const EXAMPLE_SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    #[test]
    fn payload_hash_empty_body_matches_aws_docs() {
        // e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn signing_key_derivation_matches_aws_get_example() {
        // From AWS docs GET example (service=s3, region=us-east-1, date=20130524).
        let key = derive_signing_key(EXAMPLE_SECRET_KEY, "20130524", "us-east-1", "s3");
        assert_eq!(
            hex_encode(&key),
            "dbb893acc010964918f1fd433add87c70e8b0db6be30c1fbeafefa5ec6ba8378"
        );
    }

    #[test]
    fn sign_post_headers_stable_for_fixed_inputs() {
        let credentials = AwsCredentials {
            access_key_id: EXAMPLE_ACCESS_KEY.into(),
            secret_access_key: EXAMPLE_SECRET_KEY.into(),
            session_token: None,
        };
        let body = br#"{"messages":[]}"#;
        let headers = sign_post_headers(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/converse-stream",
            body,
            &credentials,
            "us-east-1",
            "bedrock",
            "20240101T000000Z",
        )
        .unwrap();
        let auth = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20240101/us-east-1/bedrock/aws4_request"));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date"));
        assert!(auth.contains("Signature="));
        // Re-sign → identical signature (deterministic).
        let headers2 = sign_post_headers(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/converse-stream",
            body,
            &credentials,
            "us-east-1",
            "bedrock",
            "20240101T000000Z",
        )
        .unwrap();
        assert_eq!(headers, headers2);
    }

    #[test]
    fn colon_in_model_path_is_percent_encoded_in_signature() {
        let credentials = AwsCredentials {
            access_key_id: EXAMPLE_ACCESS_KEY.into(),
            secret_access_key: EXAMPLE_SECRET_KEY.into(),
            session_token: None,
        };
        let body = br#"{"messages":[]}"#;
        let headers = sign_post_headers(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-v1:0/converse-stream",
            body,
            &credentials,
            "us-east-1",
            "bedrock",
            "20240101T000000Z",
        )
        .unwrap();
        let auth = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str())
            .unwrap();
        let signature = auth.rsplit_once("Signature=").map(|(_, sig)| sig).unwrap();
        // Pinned: canonical URI encodes model-id colon as %3A.
        assert_eq!(
            signature,
            "0479dfcd1c5844fcd19ec12b535804fec1c0dd032b04195dd19ecfa04ec6f6d4"
        );
        assert_eq!(
            canonical_uri_path("/model/anthropic.claude-v1:0/converse-stream"),
            "/model/anthropic.claude-v1%3A0/converse-stream"
        );
        assert_eq!(
            canonical_uri_path("/model/anthropic.claude-v1%3A0/converse-stream"),
            "/model/anthropic.claude-v1%3A0/converse-stream"
        );
    }

    #[test]
    fn session_token_adds_security_header() {
        let credentials = AwsCredentials {
            access_key_id: EXAMPLE_ACCESS_KEY.into(),
            secret_access_key: EXAMPLE_SECRET_KEY.into(),
            session_token: Some("session-token-example".into()),
        };
        let headers = sign_post_headers(
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/x/converse-stream",
            b"{}",
            &credentials,
            "us-west-2",
            "bedrock",
            "20240101T120000Z",
        )
        .unwrap();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "x-amz-security-token" && v == "session-token-example")
        );
        let auth = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(auth.contains("x-amz-security-token"));
    }
}
