#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tracing::debug;

use alloy::rpc::client::{ClientBuilder, RpcClient as AlloyRpcClient};
use alloy::rpc::json_rpc::{RpcRecv, RpcSend}; // param/return bounds for request<P, R>
use reqwest::Url;

use crate::rpc::config::ClientConfig;
use crate::rpc::error::{Result, RpcError};
use crate::rpc::retry::run_with_retry;
use crate::rpc::transport::RetryAfterTransport;

#[derive(Debug)]
pub struct RpcClient {
    inner: AlloyRpcClient,
    semaphore: Arc<Semaphore>,
    client_config: ClientConfig
}


