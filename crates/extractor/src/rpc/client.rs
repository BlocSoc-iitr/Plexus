#![allow(dead_code)]

use std::sync::Arc;

use tokio::sync::Semaphore;

use alloy::rpc::client::{ClientBuilder, RpcClient as AlloyRpcClient};
use alloy::rpc::json_rpc::{RpcRecv, RpcSend}; // param/return bounds for request<P, R>
use reqwest::Url;

use crate::rpc::config::ClientConfig;
use crate::rpc::error::Result;
use crate::rpc::retry::run_with_retry;
use crate::rpc::transport::RetryAfterTransport;

#[derive(Debug)]
pub struct RpcClient {
    pub inner: AlloyRpcClient,
    pub semaphore: Arc<Semaphore>,
    pub client_config: ClientConfig,
}

impl RpcClient {
    pub fn new(url_str: String) -> Self {
        let client_config = ClientConfig::new_with_endpoint(&url_str);
        let url = match Url::parse(&client_config.url) {
            Ok(url) => url,
            Err(e) => {
                panic!("Invalid url: {url_str} \n Error obtained: {e}")
            }
        };
        let transport = RetryAfterTransport::new(url);
        let inner = ClientBuilder::default().transport(transport, false);
        let concurrency = client_config.max_concurrency;
        let semaphore = Arc::new(Semaphore::new(concurrency as usize));
        RpcClient {
            inner,
            semaphore,
            client_config,
        }
    }

    // skip_all so we don't try to Debug-print `self` (the whole client/config) or
    // `params` (P isn't Debug-bound); record only the method as a span field.
    #[tracing::instrument(skip_all, fields(method = %method))]
    pub async fn request<P, R>(&self, method: &str, params: P) -> Result<R>
    where
        P: RpcSend + Clone,
        R: RpcRecv,
    {
        // Permit is held for the whole call — across every retry and backoff sleep —
        // not re-acquired per attempt. Trade-off: a retrying/rate-limited call
        // keeps occupying one of `max_concurrency` slots while it sleeps, so a burst
        // of 429s can throttle throughput, in exchange a single logical request never
        // exceeds one permit and cannot be starved mid-retry by newer calls.
        let _permit = self.semaphore.clone().acquire_owned().await.unwrap();

        let op = || self.inner.request(method.to_string(), params.clone());

        run_with_retry(
            &0u32,
            method,
            &self.client_config.retry_config,
            self.client_config.timeout,
            op,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::error::RpcError;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
    use std::time::{Duration, Instant};
    use wiremock::matchers::method as http_method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    // Echo the JSON-RPC id back so alloy correlates the response (ids climb on retry).
    fn ok_body(req: &Request, result: &str) -> serde_json::Value {
        let id = serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::json!(0));
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    // First `fail_until` calls return `status` (+ optional Retry-After), then a 200 result.
    struct SeqResponder {
        calls: Arc<AtomicU32>,
        fail_until: u32,
        status: u16,
        retry_after: Option<u64>,
    }
    impl Respond for SeqResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, SeqCst);
            if n < self.fail_until {
                let mut t = ResponseTemplate::new(self.status);
                if let Some(s) = self.retry_after {
                    t = t.insert_header("retry-after", s.to_string().as_str());
                }
                t
            } else {
                ResponseTemplate::new(200).set_body_json(ok_body(req, "0x10"))
            }
        }
    }

    // happy path: a single 200 funnels through permit -> timeout -> retry -> one call.
    #[tokio::test]
    async fn smoke_single_200_returns_ok() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(SeqResponder {
                calls: Arc::new(AtomicU32::new(0)),
                fail_until: 0,
                status: 500,
                retry_after: None,
            })
            .expect(1)
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri());
        let out: Result<String> = client.request("eth_blockNumber", ()).await;
        assert_eq!(out.unwrap(), "0x10");
    }

    // transient errors retry, then succeed (exactly 3 calls: 2 fail + 1 ok).
    #[tokio::test]
    async fn transient_then_success() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicU32::new(0));
        Mock::given(http_method("POST"))
            .respond_with(SeqResponder {
                calls: calls.clone(),
                fail_until: 2,
                status: 500,
                retry_after: None,
            })
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri());
        let out: Result<String> = client.request("eth_blockNumber", ()).await;
        assert_eq!(out.unwrap(), "0x10");
        assert_eq!(calls.load(SeqCst), 3);
    }

    // persistent errors exhaust after max_attempts (3), method name preserved.
    #[tokio::test]
    async fn exhausts_with_method() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(SeqResponder {
                calls: Arc::new(AtomicU32::new(0)),
                fail_until: u32::MAX,
                status: 500,
                retry_after: None,
            })
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri());
        let out: Result<String> = client.request("eth_getLogs", ()).await;
        match out {
            Err(RpcError::RetriesExhausted {
                method,
                attempt_count,
                ..
            }) => {
                assert_eq!(method, "eth_getLogs");
                assert_eq!(attempt_count, 3);
            }
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
    }

    // a JSON-RPC error reply fails fast: returned as RpcResponse with no retries.
    #[tokio::test]
    async fn json_rpc_error_fails_fast() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": { "code": -32000, "message": "execution reverted" }
        });
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1) // exactly one call proves we did not retry a fail-fast error
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri());
        let out: Result<String> = client.request("eth_call", ()).await;
        assert!(matches!(out, Err(RpcError::RpcResponse { .. })));
    }

    // 429 + Retry-After: 1 -> the wait is floored to >= 1s, then succeeds.
    #[tokio::test]
    async fn rate_limited_retry_after_is_floored() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(SeqResponder {
                calls: Arc::new(AtomicU32::new(0)),
                fail_until: 1,
                status: 429,
                retry_after: Some(1),
            })
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri());
        let start = Instant::now();
        let out: Result<String> = client.request("eth_blockNumber", ()).await;
        assert_eq!(out.unwrap(), "0x10");
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "waited only {:?}",
            start.elapsed()
        );
    }

    // concurrency cap: in-flight requests at the server never exceed the permit count (10).
    struct CountingResponder {
        in_flight: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
        delay: Duration,
    }
    impl Respond for CountingResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.max.fetch_max(now, SeqCst);
            let inf = self.in_flight.clone();
            let delay = self.delay;
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                inf.fetch_sub(1, SeqCst);
            });
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(ok_body(req, "0x10"))
        }
    }

    #[tokio::test]
    async fn concurrency_never_exceeds_permits() {
        let server = MockServer::start().await;
        let max = Arc::new(AtomicUsize::new(0));
        Mock::given(http_method("POST"))
            .respond_with(CountingResponder {
                in_flight: Arc::new(AtomicUsize::new(0)),
                max: max.clone(),
                delay: Duration::from_millis(150),
            })
            .mount(&server)
            .await;

        let client = Arc::new(RpcClient::new(server.uri()));
        let mut handles = Vec::new();
        for _ in 0..25 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                let out: Result<String> = c.request("eth_blockNumber", ()).await;
                out.unwrap()
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), "0x10");
        }
        assert!(
            max.load(SeqCst) <= 10,
            "max in-flight was {}",
            max.load(SeqCst)
        );
    }
}
