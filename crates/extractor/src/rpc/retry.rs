#![allow(dead_code)]
use std::cmp::min;
use std::time::Duration;
use alloy::transports::TransportError;
use std::future::Future;

use rand::Rng;                 // jitter: thread_rng().gen_range(..)
use tokio::time::{sleep, timeout};        // backoff waits
use tracing::{error, warn};    // WARN per retry, ERROR on exhaustion

use crate::rpc::config::RetryConfig;
use crate::rpc::error::{Result, RpcError, classify, RetryFlag};

//calculates the delay in the successive attempts. 
pub fn delay_calc (attempt: u32, retry_config: &RetryConfig) -> Duration {
    let mut delay = retry_config.base_delay * 2u32.saturating_pow(attempt-1);
    delay = min(delay, retry_config.max_delay);
    delay
}

pub async fn run_with_retry<R, F, Fut>(
    attempt: &u32,
    method: &str,
    retry_config: &RetryConfig,
    attempt_timeout: Duration,
    mut operation: F,
) -> Result<R>
where
    F: FnMut() -> Fut,                                  // re-callable: one call = one attempt
    Fut: Future<Output = std::result::Result<R, TransportError>>,
{
    let mut attempt = attempt.clone();
    loop {
        attempt += 1;
        let outcome = timeout(attempt_timeout, operation()).await;
        match outcome {
            Ok(Ok(r))  => return Ok(r),
            Err(_elapsed) => {
                let rpc_error = RpcError::Timeout { 
                    elapsed: Some(attempt_timeout),
                    method: method.to_string(),
                };
                //before retrying, check if max attempts have exceeded.
                if attempt >= retry_config.max_attempts as u32 {
                    let rpc_error = RpcError::RetriesExhausted { 
                        method: method.to_string(), 
                        attempt_count: attempt,
                        source: Box::new(rpc_error)
                    };
                    error!(
                        method,
                        attempt,
                        err = %rpc_error,
                        "rpc attempt permanently failed, maximum attempts reached."
                    );
                    return Err(rpc_error)
                }
                // retryable, skips classify
                //computes the jitter
                let computed = delay_calc(attempt, retry_config); // retry_config is already &RetryConfig
                let high = computed.as_millis() as u64;
                let jittered = Duration::from_millis(rand::thread_rng().gen_range(0..=high));
                //warns the user before sleeping 
                warn!(
                    method,
                    attempt,
                    max_attempts = retry_config.max_attempts,
                    jitter_duration = ?jittered,
                    err = %rpc_error,
                    "rpc attempt failed, backing off and retrying"
                );
                //sleeps for the jittered duration - in the timeout error, the retry-after header is 0
                sleep(jittered).await;
                continue; //retries 
            },
            Ok(Err(e)) => {
                let (rpc_error, flag) = classify(e, method);
                //computes the jitter 
                let computed = delay_calc(attempt, retry_config); // retry_config is already &RetryConfig
                let high = computed.as_millis() as u64;
                let jittered = Duration::from_millis(rand::thread_rng().gen_range(0..=high));
                if flag == RetryFlag::Fail {// fail fast, no retry
                    error!(
                        method,
                        attempt,
                        max_attempts = retry_config.max_attempts,
                        err = %rpc_error,
                        "rpc attempt failed permanently, received error as response"
                    );
                    return Err(rpc_error)
                } 
                if attempt >= retry_config.max_attempts as u32 {
                    let rpc_error = RpcError::RetriesExhausted { 
                        method: method.to_string(), 
                        attempt_count: attempt,
                        source: Box::new(rpc_error)
                    };
                    error!(
                        method,
                        attempt,
                        err = %rpc_error,
                        "rpc attempt permanently failed, maximum attempts reached."
                    );
                    return Err(rpc_error)
                }
                else if let RpcError::RateLimited { method, retry_after } = &rpc_error {
                    //computes total duration of waiting 
                    let t_duration = jittered+retry_after.unwrap_or(Duration::from_millis(0));
                    //warns the user 
                    warn!(
                        method,
                        attempt,
                        max_attempts = retry_config.max_attempts,
                        total_duration = ?t_duration,
                        err = %rpc_error,
                        "rpc attempt failed, call got rate-limited, backing off and retrying"
                    );
                    //sleeps, then continues
                    sleep(t_duration).await;
                    continue;
                } 
                // TransportError calls classify
                {
                    warn!(
                        method,
                        attempt,
                        max_attempts = retry_config.max_attempts,
                        jitter_duration = ?jittered,
                        err = %rpc_error,
                        "rpc transport error, backing off and retrying"
                    );
                    sleep(jittered).await;
                    continue;
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::config::JitterStrategy;
    use crate::rpc::transport::RateLimited;
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::TransportErrorKind;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // Tiny delays so the real backoff sleeps don't slow the suite down.
    fn fast_cfg() -> RetryConfig {
        RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(4),
            jitter: JitterStrategy::Full,
        }
    }

    fn error_resp() -> TransportError {
        let payload = serde_json::from_str::<ErrorPayload>(
            r#"{"code":-32000,"message":"execution reverted","data":null}"#,
        )
        .unwrap();
        TransportError::ErrorResp(payload)
    }

    fn rate_limited(retry_after: Option<Duration>) -> TransportError {
        TransportErrorKind::custom(RateLimited { retry_after })
    }

    // --- delay_calc: geometric growth, then clamps at max_delay ---
    #[test]
    fn delay_calc_grows_then_clamps() {
        let cfg = RetryConfig::default(); // base 100ms, max 2s
        assert_eq!(delay_calc(1, &cfg), Duration::from_millis(100)); // 100 * 2^0
        assert_eq!(delay_calc(2, &cfg), Duration::from_millis(200)); // 100 * 2^1
        assert_eq!(delay_calc(3, &cfg), Duration::from_millis(400)); // 100 * 2^2
        assert_eq!(delay_calc(6, &cfg), Duration::from_secs(2)); // 3200ms -> clamped
    }

    // --- run_with_retry, ordered from simplest path to most involved ---

    //Succeeds on the first attempt: exactly one call, returns the value.
    #[tokio::test]
    async fn ok_on_first_attempt() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<u64, TransportError>(7)
            }
        };
        let out = run_with_retry(&0u32, "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    //A fail-fast error reply returns immediately, no retries.
    #[tokio::test]
    async fn fail_fast_does_not_retry() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<u64, TransportError>(error_resp())
            }
        };
        let out = run_with_retry(&0u32, "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert!(matches!(out, Err(RpcError::RpcResponse { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    //Transient transport errors retry, then succeed within the budget.
    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err::<u64, TransportError>(TransportErrorKind::custom_str("boom"))
                } else {
                    Ok(7)
                }
            }
        };
        let out = run_with_retry(&0u32, "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    //Persistent transport errors exhaust after exactly max_attempts calls.
    #[tokio::test]
    async fn exhausts_after_max_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<u64, TransportError>(TransportErrorKind::custom_str("boom"))
            }
        };
        let out = run_with_retry(&0u32, "m", &fast_cfg(), Duration::from_secs(5), op).await;
        match out {
            Err(RpcError::RetriesExhausted { attempt_count, .. }) => assert_eq!(attempt_count, 3),
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    //A call that never finishes within the per-attempt timeout exhausts via Timeout.
    #[tokio::test]
    async fn times_out_then_exhausts() {
        let op = || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<u64, TransportError>(7)
        };
        let out = run_with_retry(&0u32, "m", &fast_cfg(), Duration::from_millis(10), op).await;
        assert!(matches!(out, Err(RpcError::RetriesExhausted { .. })));
    }

    //A 429 is retryable: rate-limited once, then succeeds.
    #[tokio::test]
    async fn retries_rate_limited_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 1 {
                    Err::<u64, TransportError>(rate_limited(Some(Duration::from_millis(2))))
                } else {
                    Ok(7)
                }
            }
        };
        let out = run_with_retry(&0u32, "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    //A persistent 429 exhausts, and the boxed source is the RateLimited error.
    #[tokio::test]
    async fn rate_limited_exhausts_with_rate_limited_source() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<u64, TransportError>(rate_limited(Some(Duration::from_millis(2))))
            }
        };
        let out = run_with_retry(&0u32, "m", &fast_cfg(), Duration::from_secs(5), op).await;
        match out {
            Err(RpcError::RetriesExhausted { attempt_count, source, .. }) => {
                assert_eq!(attempt_count, 3);
                assert!(matches!(*source, RpcError::RateLimited { .. }));
            }
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}