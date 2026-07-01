use crate::rpc::transport::RetryAfterParseHeader;
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

    #[error("Cap hit for RPC calls: {method}, retrying after: {retry_after:?}")]
    RateLimited {
        method: String,
        retry_after: Option<Duration>,
    },

    #[error("Service unavailable currently: {method}, retrying after: {retry_after:?}")]
    ServiceUnavailable {
        method: String,
        retry_after: Option<Duration>,
    },

    #[error("Transport error on {method}, error: {source:?}")]
    Transport {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Retries exhausted on {method} after {attempt_count} attempts, error: {source:?}")]
    RetriesExhausted {
        method: String,
        attempt_count: u32,
        #[source]
        source: Box<RpcError>,
    },

    #[error("Rpc call failed with the following error:- {method}, error: {source:?}")]
    RpcResponse {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Invalid RPC url {url}: {method}")]
    InvalidUrl { url: String, method: String },

    #[error("Waiting period is very long: {retry_after:?}, failing:{method}")]
    RetryDelayTooLong {
        method: String,
        retry_after: Option<Duration>,
    },

    #[error("Unauthorized access: {method}, error: {source:?}")]
    Unauthorized {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Not found what you intend to look with: {method}, error: {source:?}")]
    NotFound {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Internal server error: {method}, error: {source:?}")]
    InternalServerError {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Bad gateway: {method}, error: {source:?}")]
    BadGateway {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Gateway timeout: {method}, error: {source:?}")]
    GatewayTimeout {
        method: String,
        #[source]
        source: TransportError,
    },

    #[error("Un-categorized error occured on method: {method}, error: {source:?}")]
    HttpStatus {
        method: String,
        #[source]
        source: TransportError,
    },
}

// the ok type varies, the error is always ours
pub type Result<T> = std::result::Result<T, RpcError>;

// turns an alloy transport error into our typed error plus a retry-or-not verdict
pub fn classify(err: TransportError, method: &str) -> (RpcError, RetryFlag) {
    let method = method.to_string();
    // json-rpc error reply, fail fast
    if err.is_error_resp() {
        return (
            RpcError::RpcResponse {
                method,
                source: err,
            },
            RetryFlag::Fail,
        );
    }
    // our custom 429 or 503 marker carries the floor
    if let Some(rl) = as_retry_after_parse_header(&err) {
        // if the wait is longer than two minutes, give up rather than hold the task
        let retry_after = rl.retry_after;
        if retry_after.unwrap_or(Duration::ZERO) > Duration::from_mins(2) {
            return (
                RpcError::RetryDelayTooLong {
                    method,
                    retry_after,
                },
                RetryFlag::Fail,
            );
        }
        if rl.status == 503 {
            return (
                RpcError::ServiceUnavailable { 
                    method,
                    retry_after 
                },
                RetryFlag::Retry
            )
        }
        return (
            RpcError::RateLimited {
                method,
                retry_after,
            },
            RetryFlag::Retry,
        );
    }
    // plain http statuses now, 429 and 503 are already dealt with above. the
    // transient server-side ones retry, a client-side 4xx won't change on a retry
    if let Some(status) = as_http_status(&err) {
        if status == 401 {
            return (
                RpcError::Unauthorized {
                    method,
                    source: err,
                },
                RetryFlag::Fail,
            );
        }
        if status == 404 {
            return (
                RpcError::NotFound {
                    method,
                    source: err,
                },
                RetryFlag::Fail,
            );
        }
        if status == 500 {
            return (
                RpcError::InternalServerError {
                    method,
                    source: err,
                },
                RetryFlag::Retry,
            );
        }
        if status == 502 {
            return (
                RpcError::BadGateway {
                    method,
                    source: err,
                },
                RetryFlag::Retry,
            );
        }
        if status == 504 {
            return (
                RpcError::GatewayTimeout {
                    method,
                    source: err,
                },
                RetryFlag::Retry,
            );
        }
        // nothing else matched. everything we retry is named above, so whatever
        // lands here we treat as not worth retrying and fail fast
        return (
            RpcError::HttpStatus {
                method,
                source: err,
            },
            RetryFlag::Fail,
        );
    }
    (
        RpcError::Transport {
            method,
            source: err,
        },
        RetryFlag::Retry,
    )
}

fn as_http_status(err: &TransportError) -> Option<u16> {
    match err {
        AlloyRpcError::Transport(kind) => kind.as_http_error().map(|h| h.status),
        _ => None,
    }
}

fn as_retry_after_parse_header(err: &TransportError) -> Option<&RetryAfterParseHeader> {
    match err {
        AlloyRpcError::Transport(TransportErrorKind::Custom(e)) => e.downcast_ref::<RetryAfterParseHeader>(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::json_rpc::ErrorPayload;
    use reqwest::StatusCode;

    // ordered to mirror classify's branch order: error-resp, then 429, then the rest

    // a json-rpc error reply is non-retryable and fails fast
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

    // a 429 marker is retryable and keeps its retry-after
    #[test]
    fn classify_rate_limited_is_retryable_and_preserves_retry_after() {
        let raw = TransportErrorKind::custom(RetryAfterParseHeader {
            status: StatusCode::TOO_MANY_REQUESTS,
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

    // a 429 with no parseable retry-after still retries, just without a floor
    #[test]
    fn classify_rate_limited_without_retry_after() {
        let raw = TransportErrorKind::custom(RetryAfterParseHeader {
            status: StatusCode::TOO_MANY_REQUESTS,
            retry_after: None,
        });
        let (err, flag) = classify(raw, "eth_call");
        assert_eq!(flag, RetryFlag::Retry);
        match err {
            RpcError::RateLimited { retry_after, .. } => assert_eq!(retry_after, None),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    // any other transport-layer failure is retryable
    #[test]
    fn classify_plain_transport_is_retryable() {
        let (err, flag) = classify(
            TransportErrorKind::custom_str("connection reset"),
            "eth_blockNumber",
        );
        assert_eq!(flag, RetryFlag::Retry);
        assert!(matches!(err, RpcError::Transport { .. }));
    }

    // a 503 marker stays retryable and keeps its retry-after floor
    #[test]
    fn classify_service_unavailable_retries_and_preserves_retry_after() {
        let raw = TransportErrorKind::custom(RetryAfterParseHeader {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: Some(Duration::from_secs(5)),
        });
        let (err, flag) = classify(raw, "eth_call");
        assert_eq!(flag, RetryFlag::Retry);
        match err {
            RpcError::ServiceUnavailable { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(5)));
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    // a retry-after past the two-minute cap fails fast instead of waiting (429 path)
    #[test]
    fn classify_rate_limited_over_cap_fails_fast() {
        let raw = TransportErrorKind::custom(RetryAfterParseHeader {
            status: StatusCode::TOO_MANY_REQUESTS,
            retry_after: Some(Duration::from_secs(3 * 60)),
        });
        let (err, flag) = classify(raw, "eth_call");
        assert_eq!(flag, RetryFlag::Fail);
        assert!(matches!(err, RpcError::RetryDelayTooLong { .. }));
    }

    // the same cap guards the 503 path (separate branch, so tested separately)
    #[test]
    fn classify_service_unavailable_over_cap_fails_fast() {
        let raw = TransportErrorKind::custom(RetryAfterParseHeader {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: Some(Duration::from_secs(3 * 60)),
        });
        let (err, flag) = classify(raw, "eth_call");
        assert_eq!(flag, RetryFlag::Fail);
        assert!(matches!(err, RpcError::RetryDelayTooLong { .. }));
    }

    // a 401 fails fast
    #[test]
    fn classify_401_unauthorized_fails_fast() {
        let (err, flag) = classify(TransportErrorKind::http_error(401, "no key".into()), "m");
        assert_eq!(flag, RetryFlag::Fail);
        assert!(matches!(err, RpcError::Unauthorized { .. }));
    }

    // a 404 fails fast
    #[test]
    fn classify_404_not_found_fails_fast() {
        let (err, flag) = classify(TransportErrorKind::http_error(404, "nope".into()), "m");
        assert_eq!(flag, RetryFlag::Fail);
        assert!(matches!(err, RpcError::NotFound { .. }));
    }

    // the transient server-side codes map to their own variants and stay retryable
    #[test]
    fn classify_500_502_504_are_retryable() {
        for code in [500u16, 502, 504] {
            let (err, flag) = classify(TransportErrorKind::http_error(code, "boom".into()), "m");
            assert_eq!(flag, RetryFlag::Retry, "status {code}");
            let matched = matches!(
                (code, &err),
                (500, RpcError::InternalServerError { .. })
                    | (502, RpcError::BadGateway { .. })
                    | (504, RpcError::GatewayTimeout { .. })
            );
            assert!(matched, "status {code} mapped wrong: {err:?}");
        }
    }

    // an unenumerated status hits the catch-all and fails fast (the 4xx-leak guard)
    #[test]
    fn classify_unenumerated_status_fails_fast() {
        let (err, flag) = classify(TransportErrorKind::http_error(403, "forbidden".into()), "m");
        assert_eq!(flag, RetryFlag::Fail);
        assert!(matches!(err, RpcError::HttpStatus { .. }));
    }
}
