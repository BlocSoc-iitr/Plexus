use std::fmt;
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use reqwest::StatusCode;
use reqwest::{header::HeaderMap, Client, Url};
use tower::Service;

// rides inside the transport error on a 429 so the retry-after value comes back
// with the request instead of through a shared side channel
#[derive(Debug)]
pub struct RetryAfterParseHeader {
    pub status: StatusCode,
    pub retry_after: Option<Duration>,
}

impl fmt::Display for RetryAfterParseHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error code:- (HTTP {})", self.status)
    }
}
impl std::error::Error for RetryAfterParseHeader {}

// only the delay-seconds form of retry-after, the http-date form is ignored
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
            // on a 429 or 503, grab retry-after and send it back in the error
            let status = resp.status();
            if status == 429 || status == 503 {
                let retry_after = parse_retry_after(resp.headers());
                return Err(TransportErrorKind::custom(RetryAfterParseHeader { 
                    status,
                    retry_after 
                }));
            }
            // anything else non-2xx (5xx, 401, 404, and so on) is a transport failure,
            // not a json-rpc reply. turn it into a typed http error so an error page
            // never reaches the json parser and gets misreported as a decode failure
            if !status.is_success() {
                // there's no retry-after to carry here, classify in error.rs sorts
                // these out by their status code
                let body = resp.bytes().await.map_err(TransportErrorKind::custom)?;
                return Err(TransportErrorKind::http_error(
                    status.as_u16(),
                    String::from_utf8_lossy(&body).into_owned(),
                ));
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

    // only the delay-seconds form is supported
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

    // call against a mock endpoint

    fn packet() -> RequestPacket {
        let req = Request::new("eth_blockNumber", Id::Number(0), ());
        RequestPacket::Single(req.serialize().unwrap())
    }

    // pull the rate-limited marker back out of a 429's transport error
    fn as_rate_limited(err: &TransportError) -> Option<&RetryAfterParseHeader> {
        match err {
            AlloyRpcError::Transport(TransportErrorKind::Custom(e)) => {
                e.downcast_ref::<RetryAfterParseHeader>()
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

    // a 429 with no header still signals rate-limited, just no floor
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

    // a non-success status comes back as a typed http error, not a decode failure
    #[tokio::test]
    async fn call_500_is_http_error_not_deser() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
            .mount(&server)
            .await;

        let mut t = RetryAfterTransport::new(server.uri().parse().unwrap());
        let err = t.call(packet()).await.expect_err("expected http error");
        match err {
            AlloyRpcError::Transport(ref kind) => {
                assert_eq!(kind.as_http_error().map(|h| h.status), Some(500));
            }
            other => panic!("expected transport http error, got {other:?}"),
        }
        assert!(as_rate_limited(&err).is_none());
    }

    // pull the service-unavailable marker back out of a 503's transport error
    fn as_service_unavailable(err: &TransportError) -> Option<&RetryAfterParseHeader> {
        match err {
            AlloyRpcError::Transport(TransportErrorKind::Custom(e)) => {
                e.downcast_ref::<RetryAfterParseHeader>()
            }
            _ => None,
        }
    }

    // a 503 carries its retry-after back in the error, not as a decode failure
    #[tokio::test]
    async fn call_503_carries_retry_after() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "9"))
            .mount(&server)
            .await;

        let mut t = RetryAfterTransport::new(server.uri().parse().unwrap());
        let err = t.call(packet()).await.expect_err("expected 503 error");
        let su = as_service_unavailable(&err).expect("expected ServiceUnavailable");
        assert_eq!(su.retry_after, Some(Duration::from_secs(9)));
    }
}
