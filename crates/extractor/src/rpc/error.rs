#![allow(dead_code)]
use std::time::Duration;
use alloy::transports::TransportError;
use thiserror::Error;
use reqwest::header::HeaderMap;

#[derive(Debug)]
pub enum RetryFlag {Fail, Retry}

#[derive(Error, Debug)]
pub enum RpcError {
    #[error("Timeout after {elapsed:?} on {method}")]
    Timeout {
        elapsed: Option<Duration>,
        method: String
    },

    #[error("Cap hit for RPC calls: {method}")]
    RateLimited {
        method: String,
        retry_after: Option<Duration>
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
    }
}

// generic over the Ok type T, error fixed to RpcError
pub type Result<T> = std::result::Result<T, RpcError>;

/// Maps an alloy transport error (+ captured 429 headers) into a typed
/// RpcError and a retryable verdict.
fn classify(
    err: TransportError,
    headers: Option<HeaderMap>,
    method: &str,
) -> (RpcError, RetryFlag) {
    // 1. Server returned a JSON-RPC error reply -> fail fast.
    if err.is_error_resp() {
        return (
            RpcError::RpcResponse { method: method.to_string(), source: err },
            RetryFlag::Fail,
        );
    } else if let Some(h) = headers {
        let retry_after = parse_retry_after(&h);
        return (
            RpcError::RateLimited {method: method.to_string(), retry_after: retry_after},
            RetryFlag::Retry
        )
    } else {
        return (
            RpcError::Transport { method: method.to_string(), source: err },
            RetryFlag::Retry
        )
    }
}

//parses the header of the return type of the retry_after error to find the 
//exact duration to wait before retrying 
fn parse_retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get("retry-after")?
        .to_str().ok()?
        .trim()
        .parse::<u64>().ok()        // <delay-seconds> form
        .map(Duration::from_secs)
}
