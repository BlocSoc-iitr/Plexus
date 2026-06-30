#![allow(dead_code)]
use std::fmt;
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use reqwest::{header::HeaderMap, Client, StatusCode, Url};
use tower::Service;

/// carried inside the transport error on a 429 so the retry-after value rides
/// back with this request, with no shared side channel and no aliasing
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

/// parse the delay-seconds form of retry-after
fn parse_retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[derive(Clone)]
pub struct RetryAfterTransport {
    client: Client,
    url: Url,
}

impl RetryAfterTransport {
    pub fn new(url: Url) -> Self {
        Self {
            client: Client::new(),
            url,
        }
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
            // on a 429, snapshot retry-after and carry it back in the error
            if resp.status() == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = parse_retry_after(resp.headers());
                return Err(TransportErrorKind::custom(RateLimited { retry_after }));
            }
            let body = resp.bytes().await.map_err(TransportErrorKind::custom)?;
            serde_json::from_slice(&body)
                .map_err(|e| TransportError::deser_err(e, String::from_utf8_lossy(&body)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::json_rpc::{Id, Request, ResponsePacket};
    use alloy::transports::RpcError as AlloyRpcError;
    use reqwest::header::HeaderValue;
    use tower::Service;
    use wiremock::matchers::method as http_method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // parse_retry_after: only the delay-seconds form is supported
    fn header_map(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn retry_after_parses_plain_seconds() {
        assert_eq!(
            parse_retry_after(&header_map("5")),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn retry_after_trims_whitespace() {
        assert_eq!(
            parse_retry_after(&header_map("  7 ")),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn retry_after_missing_is_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_non_numeric_is_none() {
        assert_eq!(parse_retry_after(&header_map("soon")), None);
    }

    #[test]
    fn retry_after_http_date_is_unsupported_none() {
        // the http-date form is intentionally not parsed
        assert_eq!(
            parse_retry_after(&header_map("Wed, 21 Oct 2015 07:28:00 GMT")),
            None
        );
    }

    // Service::call against a mock endpoint

    fn packet() -> RequestPacket {
        let req = Request::new("eth_blockNumber", Id::Number(0), ());
        RequestPacket::Single(req.serialize().unwrap())
    }

    // recover the RateLimited that rides inside a 429's transport error
    fn as_rate_limited(err: &TransportError) -> Option<&RateLimited> {
        match err {
            AlloyRpcError::Transport(TransportErrorKind::Custom(e)) => {
                e.downcast_ref::<RateLimited>()
            }
            _ => None,
        }
    }

    // a 200 with a well-formed body deserializes into a single response
    #[tokio::test]
    async fn call_200_deserializes_response() {
        let server = MockServer::start().await;
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": "0x10" });
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let mut t = RetryAfterTransport::new(server.uri().parse().unwrap());
        let resp = t.call(packet()).await.expect("expected Ok");
        assert!(matches!(resp, ResponsePacket::Single(_)));
    }

    // a 429 carries the parsed retry-after back inside the error
    #[tokio::test]
    async fn call_429_carries_retry_after() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
            .mount(&server)
            .await;

        let mut t = RetryAfterTransport::new(server.uri().parse().unwrap());
        let err = t.call(packet()).await.expect_err("expected 429 error");
        let rl = as_rate_limited(&err).expect("expected RateLimited");
        assert_eq!(rl.retry_after, Some(Duration::from_secs(7)));
    }

    // a 429 without the header still signals RateLimited, floor unknown
    #[tokio::test]
    async fn call_429_without_header_is_none() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let mut t = RetryAfterTransport::new(server.uri().parse().unwrap());
        let err = t.call(packet()).await.expect_err("expected 429 error");
        let rl = as_rate_limited(&err).expect("expected RateLimited");
        assert_eq!(rl.retry_after, None);
    }

    // a non-json body fails deserialization and is not mistaken for a 429
    #[tokio::test]
    async fn call_malformed_body_errors() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let mut t = RetryAfterTransport::new(server.uri().parse().unwrap());
        let err = t.call(packet()).await.expect_err("expected deser error");
        assert!(as_rate_limited(&err).is_none());
    }

    // a dead endpoint surfaces a transport error rather than panicking
    #[tokio::test]
    async fn call_connection_refused_is_error() {
        let mut t = RetryAfterTransport::new("http://127.0.0.1:1".parse().unwrap());
        assert!(t.call(packet()).await.is_err());
    }
}
