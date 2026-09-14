//! Read-through caching for block access lists (EIP-7928).
//!
//! Wraps the client-specific BAL fetch paths ([`fetch_reth_bal`] /
//! [`fetch_nethermind_bal`], dispatched by [`fetch_bal`]) with the same
//! disk-cache flow the block header already uses, storing each block's BAL at
//! `{chain_id}/{block_number}/bal.json`.
//!
//! [`fetch_reth_bal`]: crate::bal::fetch_reth_bal
//! [`fetch_nethermind_bal`]: crate::bal::fetch_nethermind_bal

use alloy_eip7928::AccountChanges;

use crate::bal::{fetch_bal, verify_bal_commitment, BalError, ClientKind};
use crate::cache::config::CacheConfig;
use crate::cache::io::{read_json, write_json};
use crate::cache::CacheError;
use crate::rpc::client::RpcClient;

use super::block::fetch_block_metadata;
use super::BlockId;

/// Fetches a block's access list, using the local cache when available.
///
/// The decoded `Vec<AccountChanges>` is cached rather than the normalized
/// [`BlockAccessSets`](crate::bal::BlockAccessSets), so a change to
/// normalization doesn't invalidate the cache. Because Reth (JSON) and
/// Nethermind (raw RLP) both decode into this same shape, the on-disk
/// `bal.json` is client-agnostic: a block cached via one client is reused when
/// fetched via the other.
///
/// Nothing leaves this function unverified. The block header is fetched first
/// (itself cached), and its `blockAccessListHash` is checked against the BAL on
/// both the cache-hit and the client-fetch path, so a corrupt `bal.json`, a bad
/// response, or a client bug surfaces as [`BalError::BalHashMismatch`] rather
/// than as wrong dependency edges much further downstream. Headers that carry no
/// commitment — pre-Glamsterdam blocks, clients that don't report the field —
/// skip the check, since there is nothing to check against.
///
/// Order of operations:
/// 1. Fetch the block header, which yields both the concrete block number and
///    the commitment to verify against.
/// 2. Check the cache for `bal.json` at that number.
/// 3. On hit, verify it and return it.
/// 4. On miss (or a corrupt cache file), fetch from the client, verify, write
///    the decoded BAL atomically to the cache, then return it.
///
/// A cached BAL that fails verification is treated as a corrupt cache entry, not
/// a fatal error: it falls through to a refetch that overwrites it, matching how
/// the rest of the cache layer handles damaged files. The refetched BAL is still
/// verified, so a genuinely bad BAL errors either way — the difference is only
/// whether a stale file gets a chance to self-heal.
///
/// # Errors
///
/// Returns a [`BalError`] if fetching the header, fetching from the client, or
/// writing the cache fails, or if a freshly fetched BAL does not match the
/// block's commitment. A missing or malformed cache file is not an error: it
/// falls through to a refetch, matching the rest of the cache layer.
pub async fn fetch_bal_cached(
    client: &RpcClient,
    cache: &CacheConfig,
    chain_id: u64,
    kind: ClientKind,
    block_id: BlockId,
) -> Result<Vec<AccountChanges>, BalError> {
    // the header resolves the block number and carries the commitment, so this
    // replaces the bare `resolve_block_number` call rather than adding to it
    let ctx = fetch_block_metadata(client, cache, chain_id, block_id).await?;
    let expected = ctx.block_access_list_hash;

    let path = cache.bal_path(chain_id, ctx.number);

    match read_json::<Vec<AccountChanges>>(&path) {
        // a cached BAL is only reusable if it still matches the commitment
        Ok(bal) => match expected {
            Some(expected) => match verify_bal_commitment(&bal, expected) {
                Ok(()) => return Ok(bal),
                // recoverable, so not returned as an error — but never silent,
                // since a cache entry failing its own commitment is worth knowing
                // about even when the refetch below papers over it
                Err(e) => tracing::warn!(
                    block_number = ctx.number,
                    path = %path.display(),
                    error = %e,
                    "cached bal failed commitment verification; discarding and refetching"
                ),
            },
            None => return Ok(bal),
        },
        Err(CacheError::NotFound(_)) => {} // fall through to a client fetch
        Err(CacheError::Malformed { .. }) => {} // corrupt file, refetch and overwrite
        Err(e) => return Err(e.into()),    // real IO failure, propagate
    }

    // `fetch_bal` verifies before returning, so a mismatch never reaches the cache
    let bal = fetch_bal(client, kind, &BlockId::Number(ctx.number), expected).await?;

    write_json(&path, &bal)?;

    Ok(bal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eip7928::bal::Bal;
    use alloy_eip7928::compute_block_access_list_hash;
    use alloy_primitives::B256;
    use serde_json::Value;
    use std::fs;
    use tempfile::tempdir;
    use wiremock::matchers::method as http_method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const BLOCK: u64 = 100;

    fn rpc_ok_body(req: &Request, result: Value) -> Value {
        let id = serde_json::from_slice::<Value>(&req.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::json!(0));
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    fn sample_reth_json() -> Value {
        serde_json::json!([
            {
                "address": "0x1111111111111111111111111111111111111111",
                "storageChanges": [
                    { "slot": "0x1", "changes": [ { "blockAccessIndex": "0x1", "newValue": "0x2a" } ] }
                ],
                "storageReads": ["0x7"],
                "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
                "nonceChanges": [],
                "codeChanges": []
            }
        ])
    }

    fn sample_accounts() -> Vec<AccountChanges> {
        serde_json::from_value(sample_reth_json()).unwrap()
    }

    fn sample_nethermind_hex() -> Value {
        let bal = Bal::from(sample_accounts());
        Value::String(format!("0x{}", hex::encode(alloy_rlp::encode(&bal))))
    }

    /// The commitment a block serving `sample_accounts()` would carry.
    fn sample_commitment() -> B256 {
        compute_block_access_list_hash(&sample_accounts())
    }

    /// A block header, optionally committing to a `blockAccessListHash`.
    fn block_json(bal_hash: Option<B256>) -> Value {
        let mut raw = serde_json::json!({
            "number": format!("0x{:x}", BLOCK),
            "hash": format!("0x{}", "ab".repeat(32)),
            "parentHash": format!("0x{}", "cd".repeat(32)),
            "miner": format!("0x{}", "11".repeat(20)),
            "timestamp": "0x1",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0xf4240",
            "transactions": [format!("0x{}", "ef".repeat(32))]
        });
        if let Some(h) = bal_hash {
            raw["blockAccessListHash"] = Value::String(h.to_string());
        }
        raw
    }

    /// A node that answers both the header and the BAL call.
    ///
    /// `fetch_bal_cached` now makes two different RPC calls, so the mock has to
    /// dispatch on method rather than replying with one canned body.
    async fn server(header: Value, bal: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(move |req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let result = match body["method"].as_str() {
                    Some("eth_getBlockByNumber") => header.clone(),
                    _ => bal.clone(),
                };
                ResponseTemplate::new(200).set_body_json(rpc_ok_body(req, result))
            })
            .mount(&server)
            .await;
        server
    }

    /// The common case: a node whose header commits to the BAL it serves.
    async fn honest_reth_server() -> MockServer {
        server(block_json(Some(sample_commitment())), sample_reth_json()).await
    }

    fn tmp_cache() -> (tempfile::TempDir, CacheConfig) {
        let dir = tempdir().unwrap();
        let cache = CacheConfig::with_root(dir.path().to_path_buf());
        (dir, cache)
    }

    // ── existing cache behaviour, now with verification in the path ──────────

    #[tokio::test]
    async fn cache_miss_fetches_via_rpc_and_writes_cache() {
        let server = honest_reth_server().await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        let bal = fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
            .await
            .unwrap();

        assert_eq!(bal.len(), 1);
        assert_eq!(bal[0].storage_changes[0].changes[0].block_access_index, 1);
        assert!(cache.bal_path(1, BLOCK).exists());
    }

    #[tokio::test]
    async fn cache_hit_skips_the_bal_rpc_call() {
        let server = honest_reth_server().await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
            .await
            .unwrap();
        let bal = fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
            .await
            .unwrap();

        assert_eq!(bal.len(), 1);
        // header and BAL fetched once each; the second call is served entirely
        // from disk, so verification must not cost a refetch
        let methods: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["method"].to_string())
            .collect();
        assert_eq!(methods.len(), 2);
    }

    #[tokio::test]
    async fn malformed_cache_file_triggers_refetch_and_overwrite() {
        let server = honest_reth_server().await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        let path = cache.bal_path(1, BLOCK);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not valid json").unwrap();

        let bal = fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
            .await
            .unwrap();

        assert_eq!(bal.len(), 1);
        let reread: Vec<AccountChanges> = read_json(&path).unwrap();
        assert_eq!(reread, bal);
    }

    #[tokio::test]
    async fn nethermind_cache_miss_fetches_and_writes_cache() {
        let server = server(
            block_json(Some(sample_commitment())),
            sample_nethermind_hex(),
        )
        .await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        let bal = fetch_bal_cached(
            &client,
            &cache,
            1,
            ClientKind::Nethermind,
            BlockId::Number(BLOCK),
        )
        .await
        .unwrap();

        assert_eq!(bal.len(), 1);
        assert_eq!(bal[0].storage_changes[0].changes[0].block_access_index, 1);
        assert!(cache.bal_path(1, BLOCK).exists());
    }

    #[tokio::test]
    async fn cache_is_client_agnostic_across_reth_and_nethermind() {
        let server = honest_reth_server().await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        let via_reth =
            fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
                .await
                .unwrap();
        let via_nethermind = fetch_bal_cached(
            &client,
            &cache,
            1,
            ClientKind::Nethermind,
            BlockId::Number(BLOCK),
        )
        .await
        .unwrap();

        assert_eq!(via_reth, via_nethermind);
    }

    // ── verification is enforced on the fetch path ───────────────────────────

    // The core guarantee: a node serving a BAL that doesn't match the header's
    // commitment gets an error, not a silently-accepted access list.
    #[tokio::test]
    async fn bal_not_matching_the_header_commitment_is_an_error() {
        let server = server(block_json(Some(B256::from([0x11; 32]))), sample_reth_json()).await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        let err = fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
            .await
            .unwrap_err();

        assert!(matches!(err, BalError::BalHashMismatch { .. }));
    }

    #[tokio::test]
    async fn nethermind_bal_not_matching_the_commitment_is_an_error() {
        let server = server(
            block_json(Some(B256::from([0x11; 32]))),
            sample_nethermind_hex(),
        )
        .await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        let err = fetch_bal_cached(
            &client,
            &cache,
            1,
            ClientKind::Nethermind,
            BlockId::Number(BLOCK),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, BalError::BalHashMismatch { .. }));
    }

    // A BAL that fails verification must never reach disk, or the next run would
    // read it straight back out of the cache.
    #[tokio::test]
    async fn a_mismatched_bal_is_never_cached() {
        let server = server(block_json(Some(B256::from([0x11; 32]))), sample_reth_json()).await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        let _ =
            fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK)).await;

        assert!(!cache.bal_path(1, BLOCK).exists());
    }

    // A cached BAL that no longer matches the commitment is a damaged cache
    // entry, so it's refetched and overwritten rather than returned or fatal.
    #[tokio::test]
    async fn cached_bal_failing_verification_is_refetched_and_overwritten() {
        let server = honest_reth_server().await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        // valid JSON of the right type, but not this block's access list
        let path = cache.bal_path(1, BLOCK);
        write_json(&path, &Vec::<AccountChanges>::new()).unwrap();

        let bal = fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
            .await
            .unwrap();

        assert_eq!(bal, sample_accounts());
        let reread: Vec<AccountChanges> = read_json(&path).unwrap();
        assert_eq!(reread, sample_accounts());
    }

    // With no commitment there is nothing to detect a stale entry with, so the
    // cached value is returned as-is. Documents the gap rather than hiding it.
    #[tokio::test]
    async fn cached_bal_is_returned_unverified_when_the_header_has_no_commitment() {
        let server = server(block_json(None), sample_reth_json()).await;
        let client = RpcClient::new(server.uri()).unwrap();
        let (_dir, cache) = tmp_cache();

        write_json(&cache.bal_path(1, BLOCK), &Vec::<AccountChanges>::new()).unwrap();

        let bal = fetch_bal_cached(&client, &cache, 1, ClientKind::Reth, BlockId::Number(BLOCK))
            .await
            .unwrap();

        assert!(bal.is_empty());
    }
}
