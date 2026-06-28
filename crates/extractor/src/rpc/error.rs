#![allow(dead_code)]
use std::time::Duration;
use thiserror::Error;
use alloy::transports::{RpcError as AlloyRpcError, TransportError, TransportErrorKind};
use crate::rpc::transport::RateLimited;

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
pub fn classify(
    err: TransportError,
    method: &str,
) -> (RpcError, RetryFlag) {
    // JSON-RPC error reply then fail fast
    if err.is_error_resp() {
        return (
            RpcError::RpcResponse { method: method.to_string(), source: err },
            RetryFlag::Fail,
        );
    }
    // our custom 429 signal carries the floor
    if let Some(rl) = as_rate_limited(&err) {
        let retry_after = rl.retry_after;
        return (
            RpcError::RateLimited { method: method.to_string(), retry_after },
            RetryFlag::Retry,
        );
    }
    (
        RpcError::Transport { method: method.to_string(), source: err },
        RetryFlag::Retry,
    )
}

fn as_rate_limited(err: &TransportError) -> Option<&RateLimited> {
    match err {
        AlloyRpcError::Transport(TransportErrorKind::Custom(e)) => e.downcast_ref::<RateLimited>(),
        _ => None,
    }
}
