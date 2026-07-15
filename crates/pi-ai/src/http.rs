//! HTTP boundary for provider streams.

use std::{future::Future, pin::Pin};

use serde_json::Value;

pub type HttpFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Vec<u8>>, HttpError>> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("provider returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
}

pub trait StreamHttpClient: Send + Sync {
    fn post_sse<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a Value,
    ) -> HttpFuture<'a>;

    fn post_json_stream<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a Value,
    ) -> HttpFuture<'a>;
}

#[derive(Clone, Debug)]
pub struct ReqwestStreamHttpClient {
    client: reqwest::Client,
}

impl ReqwestStreamHttpClient {
    pub fn new() -> Result<Self, HttpError> {
        // reqwest uses HTTP_PROXY/HTTPS_PROXY by default. Explicitly retaining
        // that behavior here also keeps custom test clients fully injectable.
        Ok(Self { client: reqwest::Client::builder().use_rustls_tls().build()? })
    }

    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<Vec<Vec<u8>>, HttpError> {
        let mut request = self.client.post(url).json(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(HttpError::Status {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Ok(vec![bytes.to_vec()])
    }
}

impl StreamHttpClient for ReqwestStreamHttpClient {
    fn post_sse<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a Value,
    ) -> HttpFuture<'a> {
        Box::pin(self.post(url, headers, body))
    }

    fn post_json_stream<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a Value,
    ) -> HttpFuture<'a> {
        Box::pin(self.post(url, headers, body))
    }
}
