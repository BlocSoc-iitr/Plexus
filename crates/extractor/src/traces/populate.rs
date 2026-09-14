use crate::cache::{config::CacheConfig, io, CacheError};
use crate::rpc::client::RpcClient;
use crate::traces::error::{TraceError, TraceTaskError};
use alloy_primitives::B256;
use futures::FutureExt;
use std::future::Future;
use std::sync::Arc;
use tokio::task::JoinSet;
use types::types::BlockContext;

#[derive(Debug)]
pub struct TraceFetchSummary {
    pub block_number: u64,
    pub cache_hits: Vec<B256>,
    pub fetched: Vec<B256>,
    pub failed: Vec<(B256, TraceTaskError)>,
    pub cancelled: usize,
}

impl TraceFetchSummary {
    pub fn total(&self) -> usize {
        self.cache_hits.len() + self.fetched.len() + self.failed.len() + self.cancelled
    }

    pub fn is_complete(&self) -> bool {
        self.failed.is_empty() && self.cancelled == 0
    }
}

async fn catch_task_panic(
    body: impl Future<Output = Result<(), TraceTaskError>>,
) -> Result<(), TraceTaskError> {
    match std::panic::AssertUnwindSafe(body).catch_unwind().await {
        Ok(result) => result,
        Err(_payload) => Err(TraceTaskError::TaskPanicked),
    }
}

fn partition_hashes(
    cache: &CacheConfig,
    chain_id: u64,
    block_number: u64,
    tx_hashes: Vec<B256>,
) -> (Vec<B256>, Vec<B256>) {
    let mut hits = Vec::new();
    let mut miss = Vec::new();

    for hash in tx_hashes {
        let path = cache.tx_path(chain_id, block_number, &hash);

        if path.exists() {
            hits.push(hash);
        } else {
            miss.push(hash);
        }
    }

    (hits, miss)
}

/// Concurrently fetches traces for missing txn then atomically write them to disk
/// Returns a tuple containing the successful fetched and hashes and any failures
async fn fetch_and_write_traces(
    client: Arc<RpcClient>,
    cache: Arc<CacheConfig>,
    chain_id: u64,
    block_number: u64,
    to_fetch: Vec<B256>,
) -> (Vec<B256>, Vec<(B256, TraceTaskError)>, usize) {
    let mut set: JoinSet<(B256, Result<(), TraceTaskError>)> = JoinSet::new();

    let tracer_cfg = Arc::new(serde_json::json!({
        "tracer": "prestateTracer",
        "tracerConfig": {"diffMode": true}
    }));

    for hash in to_fetch {
        let client = client.clone();
        let cache = cache.clone();

        let tracer_cfg = tracer_cfg.clone();

        set.spawn(async move {
            let result = catch_task_panic(async move {
                let hex_hash = format!("0x{}", hex::encode(hash));

                let trace_result: Result<serde_json::Value, _> = client
                    .request("debug_traceTransaction", (hex_hash, tracer_cfg))
                    .await;

                let trace = trace_result.map_err(TraceTaskError::from)?;

                let path = cache.tx_path(chain_id, block_number, &hash);
                io::write_json(&path, &trace).map_err(TraceTaskError::from)?;

                Ok(())
            })
            .await;

            (hash, result)
        });
    }

    let mut fetched = Vec::new();
    let mut failed = Vec::new();
    let mut cancelled = 0;
    while let Some(result) = set.join_next().await {
        match result {
            Ok((hash, Ok(()))) => {
                tracing::info!(?hash, "trace fetched and cached");
                fetched.push(hash);
            }
            Ok((hash, Err(reason))) => {
                tracing::warn!(?hash, %reason, "trace fetch failed");
                failed.push((hash, reason));
            }
            Err(join_err) => {
                cancelled += 1;
                tracing::error!(
                    %join_err,
                    "a trace fetch task panicked or was cancelled"
                );
            }
        }
    }
    (fetched, failed, cancelled)
}

/// Fetches and caches prestate traces for every txn in a block
///
/// Reads the cached block_header.json to get list of txn hashes
/// Already cached tx_{Hash},json files are skipped
/// Missing file are concuurently fetched by 'debug_traceTransaction'
pub async fn populate_traces(
    client: Arc<RpcClient>,
    cache: Arc<CacheConfig>,
    chain_id: u64,
    block_number: u64,
) -> Result<TraceFetchSummary, TraceError> {
    // read cache file and load the txn from header
    let header_path = cache.block_header_path(chain_id, block_number);
    let block_ctx = io::read_json::<BlockContext>(&header_path).map_err(|e| match e {
        CacheError::NotFound(_) => TraceError::BlockHeaderNotCached {
            chain_id,
            block_number,
        },
        CacheError::Malformed { .. } => TraceError::BlockHeaderMalformed {
            block_number,
            source: e,
        },
        other => TraceError::Io(other),
    })?;

    tracing::info!(
        block_number,
        tx_count = block_ctx.tx_hashes.len(),
        "starting trace population"
    );

    // Segregate the fetched and missed file
    let (cache_hits, to_fetch) =
        partition_hashes(&cache, chain_id, block_number, block_ctx.tx_hashes);

    tracing::info!(
        hits = cache_hits.len(),
        miss = to_fetch.len(),
        "cache scan complete"
    );

    // fetch missing tx_hash
    let (fetched, failed, cancelled) =
        fetch_and_write_traces(client, cache, chain_id, block_number, to_fetch).await;

    let summary = TraceFetchSummary {
        block_number,
        cache_hits,
        fetched,
        failed,
        cancelled,
    };

    tracing::info!(
        block_number,
        hits = summary.cache_hits.len(),
        fetched = summary.fetched.len(),
        failed = summary.failed.len(),
        cancelled = summary.cancelled,
        "trace population complete"
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::config::CacheConfig;
    use alloy_primitives::{Address, B256};
    use std::fs::{create_dir_all, metadata, set_permissions, write, File};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::{tempdir, TempDir};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    // Define a struct that implements Respond
    struct FailFirstResponder {
        call_count: Arc<AtomicU32>,
    }

    impl Respond for FailFirstResponder {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let n = self.call_count.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                // First call fails
                // error(404) is a permanent error thus it will not retry
                ResponseTemplate::new(404)
            } else {
                // All others succeed
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "result": { "pre": {}, "post": {} }
                }))
            }
        }
    }

    // Setting environment once
    fn setup_env() -> (TempDir, u64, u64) {
        let temp_dir = tempdir().unwrap();
        let chain_id = 1;
        let block_number = 100;

        (temp_dir, chain_id, block_number)
    }

    #[test]
    fn test_partial_cache_fetches_only_missing() {
        let (temp_dir, chain_id, block_number) = setup_env();
        let cache = CacheConfig::with_root(temp_dir.path().to_path_buf());

        let hash_cached_1 = B256::from([0x11; 32]);
        let hash_cached_2 = B256::from([0x22; 32]);
        let hash_missing = B256::from([0x33; 32]);

        let all_hashes = vec![hash_cached_1, hash_cached_2, hash_missing];

        //manually flush tx_json file
        create_dir_all(cache.block_dir(chain_id, block_number)).unwrap();
        File::create(cache.tx_path(chain_id, block_number, &hash_cached_1)).unwrap();
        File::create(cache.tx_path(chain_id, block_number, &hash_cached_2)).unwrap();

        let (hits, missed) = partition_hashes(&cache, chain_id, block_number, all_hashes);

        assert_eq!(hits.len(), 2);
        assert!(hits.contains(&hash_cached_1));
        assert!(hits.contains(&hash_cached_2));

        assert_eq!(missed[0], hash_missing);
    }

    #[tokio::test]
    async fn test_missing_block_header_returns_trace_error() {
        //temp file
        let (temp_dir, chain_id, block_number) = setup_env();

        // Added dummy URL as the test never expects to connect to it
        let client = Arc::new(RpcClient::new("http://localhost".to_string()).unwrap());
        let cache = Arc::new(CacheConfig::with_root(temp_dir.path().to_path_buf()));

        //Call populate_trace with a chain_id and block_number that has no block_header.json in that folder.
        let result = populate_traces(client, cache, chain_id, block_number).await;
        //Assert that the result is Err(TraceError::BlockHeaderNotCached { .. }).
        assert!(matches!(
            result,
            Err(TraceError::BlockHeaderNotCached { .. })
        ));
    }

    #[tokio::test]
    async fn test_no_cache_fetches_all_and_writes_to_disk() {
        //setting up mock server
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "pre": {}, "post": {} }
            })))
            .mount(&mock_server)
            .await;

        let (temp_dir, chain_id, block_number) = setup_env();

        let hash_cached_1 = B256::from([0x11; 32]);
        let hash_cached_2 = B256::from([0x22; 32]);
        let to_fetch = vec![hash_cached_1, hash_cached_2];

        let client = Arc::new(RpcClient::new(mock_server.uri()).unwrap());
        let cache = Arc::new(CacheConfig::with_root(temp_dir.path().to_path_buf()));

        let (_fetched, _failed, _cancelled) =
            fetch_and_write_traces(client, cache.clone(), chain_id, block_number, to_fetch).await;

        //checking if the cache are being writte on disc
        assert!(cache
            .tx_path(chain_id, block_number, &hash_cached_1)
            .exists());
        assert!(cache
            .tx_path(chain_id, block_number, &hash_cached_2)
            .exists());
    }

    #[tokio::test]
    async fn rpc_failure_on_one_hash_does_not_fail_others() {
        let mock_server = MockServer::start().await;
        let call_count = Arc::new(AtomicU32::new(0));
        Mock::given(method("POST"))
            .respond_with(FailFirstResponder { call_count })
            .mount(&mock_server)
            .await;

        let temp_dir = tempdir().unwrap();
        let cache = Arc::new(CacheConfig::with_root(temp_dir.path().to_path_buf()));
        let client = Arc::new(RpcClient::new(mock_server.uri()).unwrap());

        let chain_id = 1u64;
        let block_number = 100u64;

        let hash_a = B256::from([0xAA; 32]);
        let hash_b = B256::from([0xBB; 32]);
        let hash_c = B256::from([0xCC; 32]);

        let (fetched, failed, cancelled) =
            fetch_and_write_traces(client, cache.clone(), 1, 100, vec![hash_a, hash_b, hash_c])
                .await;
        // One failed, two succeeded — batch was NOT aborted
        assert_eq!(fetched.len(), 2);
        assert_eq!(failed.len(), 1);
        assert_eq!(cancelled, 0);

        let files_on_disk = [hash_a, hash_b, hash_c]
            .iter()
            .filter(|h| cache.tx_path(chain_id, block_number, h).exists())
            .count();
        assert_eq!(files_on_disk, 2);
    }
    #[tokio::test]
    async fn test_currupted_blockheader_returns_malformed_error() {
        let (temp_dir, chain_id, block_number) = setup_env();

        let client = Arc::new(RpcClient::new("http://localhost".to_string()).unwrap());
        let cache = Arc::new(CacheConfig::with_root(temp_dir.path().to_path_buf()));

        create_dir_all(cache.block_dir(chain_id, block_number)).unwrap();

        let block_path = cache.block_header_path(chain_id, block_number);

        write(&block_path, b"Corrupted data").unwrap();

        let result = populate_traces(client, cache, chain_id, block_number).await;

        assert!(matches!(
            result,
            Err(TraceError::BlockHeaderMalformed { .. })
        ));
    }

    #[tokio::test]
    async fn all_traces_already_cached_skips_rpc_and_returns_summary() {
        let (temp_dir, chain_id, block_number) = setup_env();

        let cache = Arc::new(CacheConfig::with_root(temp_dir.path().to_path_buf()));

        let hash_1 = B256::from([0x11; 32]);
        let hash_2 = B256::from([0x22; 32]);

        // Create a valid block_header.json
        let block_ctx = BlockContext {
            number: block_number,
            hash: B256::ZERO,
            parent_hash: B256::ZERO,
            coinbase: Address::ZERO,
            chain_id,
            timestamp: 1234567890,
            base_fee_per_gas: None,
            gas_limit: 30000000,
            gas_used: 15000000,
            tx_hashes: vec![hash_1, hash_2],
            block_access_list_hash: None,
        };
        io::write_json(&cache.block_header_path(chain_id, block_number), &block_ctx).unwrap();

        //  Create the two tx_{hash}.json files to pretend they are cached
        io::write_json(
            &cache.tx_path(chain_id, block_number, &hash_1),
            &serde_json::json!({}),
        )
        .unwrap();
        io::write_json(
            &cache.tx_path(chain_id, block_number, &hash_2),
            &serde_json::json!({}),
        )
        .unwrap();

        // Setup a dummy client (doesn't need a mock server because it shouldn't connect to anything)
        let client = Arc::new(RpcClient::new("http://localhost".to_string()).unwrap());

        let summary = populate_traces(client, cache, chain_id, block_number)
            .await
            .unwrap();

        assert_eq!(summary.cache_hits.len(), 2);
        assert!(summary.cache_hits.contains(&hash_1));
        assert!(summary.cache_hits.contains(&hash_2));

        assert!(summary.fetched.is_empty());
        assert!(summary.failed.is_empty());

        assert_eq!(summary.total(), 2);
        assert!(summary.is_complete());
    }

    #[tokio::test]
    // checks if it Safely catch the OS-level IO error.
    async fn fetch_succeeds_but_disk_write_fails() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "pre": {}, "post": {} }
            })))
            .mount(&mock_server)
            .await;

        let (temp_dir, chain_id, block_number) = setup_env();
        let cache = Arc::new(CacheConfig::with_root(temp_dir.path().to_path_buf()));
        let client = Arc::new(RpcClient::new(mock_server.uri()).unwrap());

        let hash = B256::from([0x11; 32]);
        let to_fetch = vec![hash];

        // Create the block directory before we run the code
        let block_dir = cache.block_dir(chain_id, block_number);
        create_dir_all(&block_dir).unwrap();

        // Lock the directory down (make it Read-Only) so write_json is guaranteed to fail
        let mut perms = metadata(&block_dir).unwrap().permissions();
        perms.set_readonly(true);
        set_permissions(&block_dir, perms).unwrap();

        // Run the inner fetch-and-write function directly
        let (fetched, failed, cancelled) =
            fetch_and_write_traces(client, cache.clone(), chain_id, block_number, to_fetch).await;

        assert!(fetched.is_empty());
        assert_eq!(failed.len(), 1);
        assert_eq!(cancelled, 0);

        // Prove exactly WHY it failed using the new TraceTaskError enum
        let (failed_hash, error_reason) = &failed[0];
        assert_eq!(failed_hash, &hash);
        assert!(matches!(error_reason, TraceTaskError::CacheWrite { .. }));
    }

    #[tokio::test]
    async fn catch_task_panic_converts_panic_into_task_panicked_error() {
        let result = catch_task_panic(async { panic!("boom") }).await;

        assert!(matches!(result, Err(TraceTaskError::TaskPanicked)));
    }

    #[tokio::test]
    async fn catch_task_panic_preserves_successful_result() {
        let result = catch_task_panic(async { Ok(()) }).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn catch_task_panic_preserves_regular_error() {
        let err = CacheError::NotFound(std::path::PathBuf::from("/nope"));
        let result = catch_task_panic(async move { Err(TraceTaskError::from(err)) }).await;

        assert!(matches!(result, Err(TraceTaskError::CacheWrite { .. })));
    }

    #[test]
    fn summary_counts_cancelled_as_incomplete() {
        let summary = TraceFetchSummary {
            block_number: 1,
            cache_hits: vec![B256::from([0x11; 32])],
            fetched: vec![B256::from([0x22; 32])],
            failed: vec![],
            cancelled: 1,
        };

        assert_eq!(summary.total(), 3);
        assert!(!summary.is_complete());
    }

    #[test]
    fn summary_with_cancelled_and_failed_is_incomplete() {
        let summary = TraceFetchSummary {
            block_number: 1,
            cache_hits: vec![],
            fetched: vec![B256::from([0x22; 32])],
            failed: vec![(B256::from([0x33; 32]), TraceTaskError::TaskPanicked)],
            cancelled: 1,
        };

        assert_eq!(summary.total(), 3);
        assert!(!summary.is_complete());
    }
}
