#![allow(dead_code)]
use std::fmt;
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use reqwest::{header::HeaderMap, Client, StatusCode, Url};
use tower::Service;

/// Carried inside the transport error on a 429, so the Retry-After value rides
/// back with this request — no shared side channel, no aliasing.
#[derive(Debug)]
pub struct RateLimited {
    pub retry_after: Option<Duration>,
}

impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rate limited (HTTP 429)")
    }
}
impl std::error::Error for RateLimited {}

/// Parse the <delay-seconds> form of Retry-After.
fn parse_retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get("retry-after")?
        .to_str().ok()?
        .trim()
        .parse::<u64>().ok()
        .map(Duration::from_secs)
}

#[derive(Clone)]
pub struct RetryAfterTransport {
    client: Client,
    url: Url,
}

impl RetryAfterTransport {
    pub fn new(url: Url) -> Self {
        Self { client: Client::new(), url }
    }
}

impl Service<RequestPacket> for RetryAfterTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(())) // reqwest pools internally; always ready
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let client = self.client.clone();
        let url = self.url.clone();
        Box::pin(async move {
            let resp = client
                .post(url)
                .json(&req)
                .send()
                .await
                .map_err(TransportErrorKind::custom)?;
            // On a 429, snapshot Retry-After and carry it back *in the error*.
            if resp.status() == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = parse_retry_after(resp.headers());
                return Err(TransportErrorKind::custom(RateLimited { retry_after }));
            }
            let body = resp.bytes().await.map_err(TransportErrorKind::custom)?;
            serde_json::from_slice(&body).map_err(|e| TransportError::deser_err(e, String::from_utf8_lossy(&body)))
        })
    }
}