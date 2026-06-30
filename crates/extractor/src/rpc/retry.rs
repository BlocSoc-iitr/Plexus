use alloy::transports::TransportError;
use std::cmp::min;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use rand::Rng;
use tokio::time::{sleep, timeout};
use tracing::{error, warn};

use crate::rpc::config::RetryConfig;
use crate::rpc::error::{classify, Result, RetryFlag};
use crate::rpc::RpcError;

// delay for the given attempt, doubling each time and clamped at max_delay
fn delay_calc(attempt: u32, retry_config: &RetryConfig) -> Duration {
    let mut delay = retry_config.base_delay * 2u32.saturating_pow(attempt - 1);
    delay = min(delay, retry_config.max_delay);
    delay
}

// full jitter over [0, computed]. split out so the bound is unit-testable
fn apply_jitter(computed: Duration, rng: &mut impl Rng) -> Duration {
    let high = computed.as_millis() as u64;
    Duration::from_millis(rng.gen_range(0..=high))
}

// total wait: full jitter over the computed backoff, with retry-after added
// on top as a floor (none means just the jittered delay)
fn backoff_wait(
    attempt: u32,
    cfg: &RetryConfig,
    retry_after: Option<Duration>,
    rng: &mut impl Rng,
) -> Duration {
    apply_jitter(delay_calc(attempt, cfg), rng) + retry_after.unwrap_or(Duration::ZERO)
}

pub async fn run_with_retry<R, F, Fut>(
    semaphore: &Arc<Semaphore>,
    method: &str,
    retry_config: &RetryConfig,
    attempt_timeout: Duration,
    mut operation: F,
) -> Result<R>
where
    F: FnMut() -> Fut, // re-callable: each call is one attempt
    Fut: Future<Output = std::result::Result<R, TransportError>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let permit = semaphore.acquire().await.unwrap();
        let outcome = timeout(attempt_timeout, operation()).await;
        match outcome {
            Ok(Ok(r)) => return Ok(r),
            Err(_elapsed) => {
                let rpc_error = RpcError::Timeout {
                    elapsed: Some(attempt_timeout),
                    method: method.to_string(),
                };
                // before retrying, check whether max attempts have been reached
                if attempt >= retry_config.max_attempts as u32 {
                    let rpc_error = RpcError::RetriesExhausted {
                        method: method.to_string(),
                        attempt_count: attempt,
                        source: Box::new(rpc_error),
                    };
                    error!(
                        method,
                        attempt,
                        err = %rpc_error,
                        "rpc attempt permanently failed, maximum attempts reached."
                    );
                    return Err(rpc_error);
                }
                // a timeout is always worth another go, no need to classify it
                let jittered = backoff_wait(attempt, retry_config, None, &mut rand::thread_rng());
                warn!(
                    method,
                    attempt,
                    max_attempts = retry_config.max_attempts,
                    jitter_duration = ?jittered,
                    err = %rpc_error,
                    "rpc attempt failed, backing off and retrying"
                );
                // release the permit before sleeping so a backing-off call frees its slot
                drop(permit);
                sleep(jittered).await;
                continue;
            }
            Ok(Err(e)) => {
                let (rpc_error, flag) = classify(e, method);
                if flag == RetryFlag::Fail {
                    // fail fast, no retry
                    error!(
                        method,
                        attempt,
                        max_attempts = retry_config.max_attempts,
                        err = %rpc_error,
                        "rpc attempt failed permanently, received error as response"
                    );
                    return Err(rpc_error);
                }
                if attempt >= retry_config.max_attempts as u32 {
                    let rpc_error = RpcError::RetriesExhausted {
                        method: method.to_string(),
                        attempt_count: attempt,
                        source: Box::new(rpc_error),
                    };
                    error!(
                        method,
                        attempt,
                        err = %rpc_error,
                        "rpc attempt permanently failed, maximum attempts reached."
                    );
                    return Err(rpc_error);
                }
                // rate-limited or service-unavailable, both hand us a retry-after to wait out
                if let RpcError::RateLimited {
                    method,
                    retry_after,
                }
                | RpcError::ServiceUnavailable {
                    method,
                    retry_after,
                } = &rpc_error
                {
                    let t_duration =
                        backoff_wait(attempt, retry_config, *retry_after, &mut rand::thread_rng());
                    warn!(
                        method,
                        attempt,
                        max_attempts = retry_config.max_attempts,
                        total_duration = ?t_duration,
                        err = %rpc_error,
                        "rpc attempt failed, backing off and retrying"
                    );
                    // release the permit before sleeping so the slot is free during backoff
                    drop(permit);
                    sleep(t_duration).await;
                    continue;
                }
                // the retryable server-side statuses, no retry-after to honour so we
                // just back off on our own clock
                if let RpcError::BadGateway {
                    method,
                    source: err,
                }
                | RpcError::GatewayTimeout {
                    method,
                    source: err,
                }
                | RpcError::InternalServerError {
                    method,
                    source: err,
                } = &rpc_error
                {
                    let t_duration =
                        backoff_wait(attempt, retry_config, None, &mut rand::thread_rng());
                    warn!(
                        method,
                        attempt,
                        max_attempts = retry_config.max_attempts,
                        total_duration = ?t_duration,
                        err = %err,
                        "rpc attempt failed, backing off and retrying"
                    );
                    // release the permit before sleeping so the slot is free during backoff
                    drop(permit);
                    sleep(t_duration).await;
                    continue;
                }
                // plain transport error, back off and retry
                {
                    let jittered =
                        backoff_wait(attempt, retry_config, None, &mut rand::thread_rng());
                    warn!(
                        method,
                        attempt,
                        max_attempts = retry_config.max_attempts,
                        jitter_duration = ?jittered,
                        err = %rpc_error,
                        "rpc transport error, backing off and retrying"
                    );
                    drop(permit);
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
    use crate::rpc::transport::{RateLimited, ServiceUnavailable};
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::TransportErrorKind;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // tiny delays so the real backoff sleeps don't slow the suite down
    fn fast_cfg() -> RetryConfig {
        RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(4),
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

    fn service_unavailable(retry_after: Option<Duration>) -> TransportError {
        TransportErrorKind::custom(ServiceUnavailable { retry_after })
    }

    // a roomy semaphore so these single-flight retry tests never block on permits;
    // the release-and-reacquire path is exercised regardless of capacity
    fn sem() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(10))
    }

    // delay_calc: geometric growth, then clamps at max_delay
    #[test]
    fn delay_calc_grows_then_clamps() {
        let cfg = RetryConfig::default(); // base 100ms, max 2s
        assert_eq!(delay_calc(1, &cfg), Duration::from_millis(100)); // 100 * 2^0
        assert_eq!(delay_calc(2, &cfg), Duration::from_millis(200)); // 100 * 2^1
        assert_eq!(delay_calc(3, &cfg), Duration::from_millis(400)); // 100 * 2^2
        assert_eq!(delay_calc(6, &cfg), Duration::from_secs(2)); // clamped
    }

    // run_with_retry tests, simplest path first

    // succeeds on the first attempt: one call, returns the value
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
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // a fail-fast error reply returns immediately, no retries
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
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert!(matches!(out, Err(RpcError::RpcResponse { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // transient transport errors retry, then succeed within the budget
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
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    // persistent transport errors exhaust after max_attempts calls
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
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        match out {
            Err(RpcError::RetriesExhausted { attempt_count, .. }) => assert_eq!(attempt_count, 3),
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    // a call that never finishes within the per-attempt timeout exhausts as a timeout
    #[tokio::test]
    async fn times_out_then_exhausts() {
        let op = || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<u64, TransportError>(7)
        };
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_millis(10), op).await;
        assert!(matches!(out, Err(RpcError::RetriesExhausted { .. })));
    }

    // a timed-out attempt is retryable: it times out once, then the next call succeeds
    #[tokio::test]
    async fn times_out_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    tokio::time::sleep(Duration::from_secs(30)).await; // blow the timeout once
                }
                Ok::<u64, TransportError>(7)
            }
        };
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_millis(10), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // a 429 is retryable: rate-limited once, then succeeds
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
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // a 503 is retryable too: unavailable once, then succeeds. covers the
    // service-unavailable branch, separate from the 429 path above
    #[tokio::test]
    async fn retries_service_unavailable_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 1 {
                    Err::<u64, TransportError>(service_unavailable(Some(Duration::from_millis(2))))
                } else {
                    Ok(7)
                }
            }
        };
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // a retry-after past the two-minute cap is non-retryable: classify bails, so
    // run_with_retry returns after one attempt instead of sleeping for minutes
    #[tokio::test]
    async fn retry_after_over_cap_fails_fast_without_retry() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let op = move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<u64, TransportError>(service_unavailable(Some(Duration::from_secs(3 * 60))))
            }
        };
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        assert!(matches!(out, Err(RpcError::RetryDelayTooLong { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // a persistent 429 exhausts, and the boxed source is the rate-limited error
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
        let out = run_with_retry(&sem(), "m", &fast_cfg(), Duration::from_secs(5), op).await;
        match out {
            Err(RpcError::RetriesExhausted {
                attempt_count,
                source,
                ..
            }) => {
                assert_eq!(attempt_count, 3);
                assert!(matches!(*source, RpcError::RateLimited { .. }));
            }
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    // a backing-off attempt has to drop its permit so others can run during the
    // sleep. with a single permit, the first call grabs it and backs off ~200ms on
    // a 429; the second has to slip through and finish before the first wakes.
    // holding the permit across the sleep would flip that ordering
    #[tokio::test]
    async fn permit_released_during_backoff() {
        let sem = Arc::new(Semaphore::new(1));
        let order = Arc::new(std::sync::Mutex::new(Vec::<&str>::new()));
        let a_has_permit = Arc::new(tokio::sync::Notify::new());

        let a = {
            let (sem, order, signal) = (sem.clone(), order.clone(), a_has_permit.clone());
            let calls = Arc::new(AtomicU32::new(0));
            tokio::spawn(async move {
                let op = move || {
                    let (calls, signal) = (calls.clone(), signal.clone());
                    async move {
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            signal.notify_one(); // this call now holds the only permit
                            Err::<u64, TransportError>(rate_limited(Some(Duration::from_millis(
                                200,
                            ))))
                        } else {
                            Ok(7)
                        }
                    }
                };
                run_with_retry(&sem, "A", &fast_cfg(), Duration::from_secs(5), op)
                    .await
                    .unwrap();
                order.lock().unwrap().push("A");
            })
        };

        // start the second call only once the first provably holds the permit and is backing off
        a_has_permit.notified().await;
        let b = {
            let (sem, order) = (sem.clone(), order.clone());
            tokio::spawn(async move {
                let op = || async { Ok::<u64, TransportError>(7) };
                run_with_retry(&sem, "B", &fast_cfg(), Duration::from_secs(5), op)
                    .await
                    .unwrap();
                order.lock().unwrap().push("B");
            })
        };

        a.await.unwrap();
        b.await.unwrap();
        assert_eq!(*order.lock().unwrap(), ["B", "A"]);
    }

    // full jitter never escapes [0, computed]; retry-after only shifts the floor up
    #[test]
    fn jitter_stays_within_bounds() {
        let cfg = RetryConfig::default();
        let mut rng = rand::thread_rng();

        // across the growth range and the cap, jitter stays under the computed delay
        for attempt in 1..=6 {
            let computed = delay_calc(attempt, &cfg);
            for _ in 0..1_000 {
                assert!(apply_jitter(computed, &mut rng) <= computed);
            }
        }

        // with a retry-after floor, total wait lands in [retry_after, retry_after + computed]
        let ra = Duration::from_secs(5);
        let computed = delay_calc(2, &cfg);
        for _ in 0..1_000 {
            let w = backoff_wait(2, &cfg, Some(ra), &mut rng);
            assert!(w >= ra && w <= ra + computed);
        }
    }
}
