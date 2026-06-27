#![allow(dead_code)]

use std::time::Duration;

#[derive(PartialEq, Debug)]
pub enum JitterStrategy {Full} //Jitter_Strategy enum - defauts to Full jitter for better spread

pub struct RetryConfig {
    pub max_attempts: u8,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter: JitterStrategy
}

pub struct ClientConfig {
    pub url: String,
    pub max_concurrency: u32,
    pub retry_config: RetryConfig,
    pub timeout: Duration,
}

impl RetryConfig {
    //Default setting for a RetryConfig
    pub fn default() -> Self {
        let def = RetryConfig {
            max_attempts: 3,
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
            retry_config: RetryConfig::default(),
            timeout: Duration::from_secs(5)
        };
        def
    }

    //ClientConfig setup with a url supplied by the client
    pub fn default_with_endpoint(url: &String) -> Self {
        let def = ClientConfig {
            url: url.clone(),
            max_concurrency: 20,
            retry_config: RetryConfig::default(),
            timeout: Duration::from_secs(5)
        };
        def
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn clientconfig_default_returns_correct_params() {
        let client_config = ClientConfig::default();
        
        //If these basic params are correct, other params will also be 
        //correct by default. 
        assert_eq!(client_config.retry_config.max_attempts,3);
        assert_eq!(client_config.max_concurrency, 20);
        assert_eq!(client_config.retry_config.jitter, JitterStrategy::Full);
    }

    #[test]
    fn clientconfig_url_with_default_returns_correct_params() {
        let url = String::from("https://ethereum-rpc.publicnode.com");
        let client_config = ClientConfig::default_with_endpoint(&url);

        assert_eq!(client_config.url, url);

        //If these basic params are correct, other params will also be 
        //correct by default. 
        assert_eq!(client_config.retry_config.max_attempts,3);
        assert_eq!(client_config.max_concurrency, 20);
        assert_eq!(client_config.retry_config.jitter, JitterStrategy::Full);
    }
}