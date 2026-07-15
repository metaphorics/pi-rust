//! HTTP boundary for provider streams.

use std::{future::Future, pin::Pin};

use futures_util::{Stream, StreamExt};
use serde_json::Value;

pub type HttpByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpError>> + Send>>;
pub type HttpFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HttpByteStream, HttpError>> + Send + 'a>>;

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
        Ok(Self {
            client: reqwest::Client::builder().use_rustls_tls().build()?,
        })
    }

    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpByteStream, HttpError> {
        let mut request = self.client.post(url).json(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await?;
            return Err(HttpError::Status {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Ok(Box::pin(response.bytes_stream().map(|chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(HttpError::Request)
        })))
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
