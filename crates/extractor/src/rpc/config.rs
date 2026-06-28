use std::time::Duration;

#[derive(Debug)]
pub struct RetryConfig {
    pub max_attempts: u8,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

#[derive(Debug)]
pub struct ClientConfig {
    pub url: String,
    pub max_concurrency: u32,
    pub retry_config: RetryConfig,
    pub timeout: Duration,
}

impl RetryConfig {
    //Default setting for a RetryConfig
    pub fn new() -> Self {
        RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        }
    }
}

impl ClientConfig {
    //ClientConfig setup with a url supplied by the user
    pub fn new_with_endpoint(url: &str) -> Self {
        ClientConfig {
            url: url.to_string(),
            max_concurrency: 10,
            retry_config: RetryConfig::new(),
            timeout: Duration::from_secs(5),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn clientconfig_url_with_default_returns_correct_params() {
        let url = String::from("https://ethereum-rpc.publicnode.com");
        let client_config = ClientConfig::new_with_endpoint(&url);

        assert_eq!(client_config.url, url);

        //If these basic params are correct, other params will also be
        //correct by default.
        assert_eq!(client_config.retry_config.max_attempts, 3);
        assert_eq!(client_config.max_concurrency, 10);
        assert_eq!(client_config.timeout, Duration::from_secs(5));
    }
}
