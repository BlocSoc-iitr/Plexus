#![allow(dead_code)]
use std::time::Duration;
use alloy::transports::TransportError;
use thiserror::Error;

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
        method: String
    }
}
