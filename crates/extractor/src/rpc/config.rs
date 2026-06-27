#![allow(dead_code)]

use std::time::Duration;

use crate::rpc::config::JitterStrategy::Full;

enum JitterStrategy {Full, Equal}

pub struct RetryConfig {
    pub max_attemps: u8,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter: JitterStrategy
}

impl RetryConfig {
    //Default settings for a RetryConfig
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