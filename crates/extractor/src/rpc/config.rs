#![allow(dead_code)]

use std::time::Duration;

use crate::rpc::config::JitterStrategy::Full;

enum JitterStrategy {Full} //Jitter_Strategy enum - defauts to Full jitter for better spread

pub struct RetryConfig {
    pub max_attemps: u8,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter: JitterStrategy
}

pub struct ClientConfig {
    pub url: String,
    pub max_concurrency: u32,
    pub retry_config: RetryConfig
}

impl RetryConfig {
    //Default setting for a RetryConfig
    pub fn default() -> Self {
        let def = RetryConfig {
            max_attemps: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            jitter: JitterStrategy::Full
        };
        def
    }
}

impl ClientConfig {
    //Default setting for a ClientConfig
    pub fn default() -> Self {
        let def = ClientConfig {
            url: String::from("http://localhost:8545"),
            max_concurrency: 20,
            retry_config: RetryConfig::default()
        };
        def
    }

    //ClientConfig setup with a url supplied by the client
    pub fn new_with_endpoint(url: &String) -> Self {
        let def = ClientConfig {
            url: url.clone(),
            max_concurrency: 20,
            retry_config: RetryConfig::default()
        };
        def
    }
}