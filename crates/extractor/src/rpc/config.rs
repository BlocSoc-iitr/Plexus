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

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        }
    }
}

impl ClientConfig {
    pub fn new(url: &str) -> Self {
        ClientConfig {
            url: url.to_string(),
            max_concurrency: 10,
            retry_config: RetryConfig::default(),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_concurrency(mut self, n: u32) -> Self {
        self.max_concurrency = n;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn clientconfig_url_with_default_returns_correct_params() {
        let url = String::from("https://ethereum-rpc.publicnode.com");
        let client_config = ClientConfig::new(&url);

        assert_eq!(client_config.url, url);

        // if these basic params are correct, the others are correct by default too
        assert_eq!(client_config.retry_config.max_attempts, 3);
        assert_eq!(client_config.max_concurrency, 10);
        assert_eq!(client_config.timeout, Duration::from_secs(30));
    }

    #[test]
    fn with_concurrency_overrides_default() {
        let client_config =
            ClientConfig::new("https://ethereum-rpc.publicnode.com").with_concurrency(50);

        assert_eq!(client_config.max_concurrency, 50);
    }

    #[test]
    fn with_timeout_overrides_default() {
        let client_config = ClientConfig::new("https://ethereum-rpc.publicnode.com")
            .with_timeout(Duration::from_secs(5));

        assert_eq!(client_config.timeout, Duration::from_secs(5));
    }
}
