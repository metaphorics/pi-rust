use serde::{Deserialize, Serialize};

use crate::{
    auth::OAuthCredential,
    types::{ModelCost, ModelInput, ThinkingLevelMap},
};

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
pub const CALLBACK_HOST: &str = "127.0.0.1";
pub const CALLBACK_PORT: u16 = 1456;
pub const CALLBACK_PATH: &str = "/oauth/callback";
pub const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";
pub const TOKEN_EXPIRY_SKEW_MS: i64 = 60_000;
pub const LOGIN_METHOD_BROWSER: &str = "browser";
pub const LOGIN_METHOD_DEVICE_CODE: &str = "device-code";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInput>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayConfig {
    pub base_url: String,
    pub models: Vec<RadiusGatewayModel>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusOAuthCredentials {
    #[serde(flatten)]
    pub credential: OAuthCredential,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_config: Option<RadiusGatewayConfig>,
}
