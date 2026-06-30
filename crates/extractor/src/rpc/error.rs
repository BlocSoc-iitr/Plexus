#![allow(dead_code)]
use crate::rpc::transport::RateLimited;
use alloy::transports::{RpcError as AlloyRpcError, TransportError, TransportErrorKind};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, PartialEq)]
pub enum RetryFlag {
    Fail,
    Retry,
}

#[derive(Error, Debug)]
pub enum RpcError {
    #[error("Timeout after {elapsed:?} on {method}")]
    Timeout {
        elapsed: Option<Duration>,
        method: String,
    },

    #[error("Cap hit for RPC calls: {method}")]
    RateLimited {
        method: String,
        retry_after: Option<Duration>,
    },

    #[error("Transport error on {method}")]
    Transport {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Retries exhausted on {method} after {attempt_count} attempts")]
    RetriesExhausted {
        method: String,
        attempt_count: u32,
        #[source]
        source: Box<RpcError>,
    },

    #[error("Rpc call failed with the following error:- {method}")]
    RpcResponse {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Invalid RPC url {url}: {method}")]
    InvalidUrl { url: String, method: String },
}

// generic over the ok type T, error fixed to RpcError
pub type Result<T> = std::result::Result<T, RpcError>;

/// maps an alloy transport error and captured 429 headers into a typed
/// RpcError and a retryable verdict
pub fn classify(err: TransportError, method: &str) -> (RpcError, RetryFlag) {
    // json-rpc error reply, fail fast
    if err.is_error_resp() {
        return (
            RpcError::RpcResponse {
                method: method.to_string(),
                source: err,
            },
            RetryFlag::Fail,
        );
    }
    // our custom 429 signal carries the floor
    if let Some(rl) = as_rate_limited(&err) {
        let retry_after = rl.retry_after;
        return (
            RpcError::RateLimited {
                method: method.to_string(),
                retry_after,
            },
            RetryFlag::Retry,
        );
    }
    (
        RpcError::Transport {
            method: method.to_string(),
            source: err,
        },
        RetryFlag::Retry,
    )
}

fn as_rate_limited(err: &TransportError) -> Option<&RateLimited> {
    match err {
        AlloyRpcError::Transport(TransportErrorKind::Custom(e)) => e.downcast_ref::<RateLimited>(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::json_rpc::ErrorPayload;

    // ordered to mirror classify's own branch order: error-resp, then 429, then plain transport

    // a json-rpc error reply is non-retryable and maps to RpcResponse
    #[test]
    fn classify_error_response_fails_fast() {
        let payload = serde_json::from_str::<ErrorPayload>(
            r#"{"code":-32000,"message":"execution reverted","data":null}"#,
        )
        .unwrap();
        let (err, flag) = classify(TransportError::ErrorResp(payload), "eth_call");
        assert_eq!(flag, RetryFlag::Fail);
        assert!(matches!(err, RpcError::RpcResponse { .. }));
    }

    // a 429 carried as a custom RateLimited error is retryable and keeps retry_after
    #[test]
    fn classify_rate_limited_is_retryable_and_preserves_retry_after() {
        let raw = TransportErrorKind::custom(RateLimited {
            retry_after: Some(Duration::from_secs(2)),
        });
        let (err, flag) = classify(raw, "eth_getLogs");
        assert_eq!(flag, RetryFlag::Retry);
        match err {
            RpcError::RateLimited {
                method,
                retry_after,
            } => {
                assert_eq!(method, "eth_getLogs");
                assert_eq!(retry_after, Some(Duration::from_secs(2)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    // a 429 with no or unparseable retry-after still classifies as RateLimited (retry_after none)
    #[test]
    fn classify_rate_limited_without_retry_after() {
        let raw = TransportErrorKind::custom(RateLimited { retry_after: None });
        let (err, flag) = classify(raw, "eth_call");
        assert_eq!(flag, RetryFlag::Retry);
        match err {
            RpcError::RateLimited { retry_after, .. } => assert_eq!(retry_after, None),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    // any other transport-layer failure is retryable and maps to Transport
    #[test]
    fn classify_plain_transport_is_retryable() {
        let (err, flag) = classify(
            TransportErrorKind::custom_str("connection reset"),
            "eth_blockNumber",
        );
        assert_eq!(flag, RetryFlag::Retry);
        assert!(matches!(err, RpcError::Transport { .. }));
    }
}
