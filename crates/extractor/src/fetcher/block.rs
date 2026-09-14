use alloy_primitives::{Address, B256};
use serde_json::Value;
use std::num::ParseIntError;
use std::result;
use std::str::FromStr;

use super::error::FetchError;
use crate::cache::config::CacheConfig;
use crate::cache::io::{read_json, write_json};
use crate::cache::CacheError;
use crate::rpc::client::RpcClient;
use types::types::BlockContext;

#[derive(Debug, Clone)]
pub enum BlockId {
    Number(u64),
    Tag(String), // whether it is the latest, final , safe or pending
}

impl BlockId {
    pub(crate) fn as_rpc_param(&self) -> String {
        match self {
            BlockId::Number(n) => format!("0x{:x}", n),
            BlockId::Tag(t) => t.clone(),
        }
    }
}

type Result<T> = result::Result<T, FetchError>;

async fn raw_get_block_by_number(client: &RpcClient, block_id: &BlockId) -> Result<Value> {
    let raw: Option<Value> = client
        .request("eth_getBlockByNumber", (block_id.as_rpc_param(), false))
        .await?;

    raw.ok_or_else(|| FetchError::BlockNotFound(block_id.clone()))
}

// tag (latest , final etc ) to a concrete block number.
// if block_id is already a number then it will directly return with no rpc call

pub(crate) async fn resolve_block_number(client: &RpcClient, block_id: &BlockId) -> Result<u64> {
    match block_id {
        BlockId::Number(n) => Ok(*n),
        BlockId::Tag(_) => {
            let raw = raw_get_block_by_number(client, block_id).await?;
            extract_u64(&raw, "number")
        }
    }
}

//field extraction helpers

fn extract_u64(raw: &Value, field: &'static str) -> Result<u64> {
    let s = raw
        .get(field)
        .and_then(Value::as_str)
        .ok_or(FetchError::MalformedResponse { field })?;
    parse_hex_u64(s).map_err(|_| FetchError::MalformedResponse { field })
}

fn extract_b256(raw: &Value, field: &'static str) -> Result<B256> {
    let s = raw
        .get(field)
        .and_then(Value::as_str)
        .ok_or(FetchError::MalformedResponse { field })?;
    B256::from_str(s).map_err(|_| FetchError::MalformedResponse { field })
}

/// Extracts a `B256` field that may legitimately be absent.
///
/// A missing key and an explicit JSON `null` both read as `None` — clients
/// report the latter for blocks before the field's fork activated. A present
/// but unparseable value is still a malformed response.
fn extract_opt_b256(raw: &Value, field: &'static str) -> Result<Option<B256>> {
    match raw.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let s = v.as_str().ok_or(FetchError::MalformedResponse { field })?;
            B256::from_str(s)
                .map(Some)
                .map_err(|_| FetchError::MalformedResponse { field })
        }
    }
}

fn extract_address(raw: &Value, field: &'static str) -> Result<Address> {
    let s = raw
        .get(field)
        .and_then(Value::as_str)
        .ok_or(FetchError::MalformedResponse { field })?;
    Address::from_str(s).map_err(|_| FetchError::MalformedResponse { field })
}

fn parse_hex_u64(s: &str) -> result::Result<u64, ParseIntError> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
}

fn parse_hex_u128(s: &str) -> Result<u128> {
    u128::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| {
        FetchError::MalformedResponse {
            field: "baseFeePerGas",
        }
    })
}

fn parse_block_context(raw: &Value, chain_id: u64) -> Result<BlockContext> {
    let number = extract_u64(raw, "number")?;
    let hash = extract_b256(raw, "hash")?;
    let parent_hash = extract_b256(raw, "parentHash")?;
    let coinbase = extract_address(raw, "miner")?;
    let timestamp = extract_u64(raw, "timestamp")?;
    let gas_limit = extract_u64(raw, "gasLimit")?;
    let gas_used = extract_u64(raw, "gasUsed")?;

    // generally base fee is absent on pre-EIP-1559 blocks
    let base_fee_per_gas = raw
        .get("baseFeePerGas")
        .and_then(Value::as_str)
        .map(parse_hex_u128)
        .transpose()?;

    // absent on pre-Glamsterdam blocks and on clients that don't report it
    let block_access_list_hash = extract_opt_b256(raw, "blockAccessListHash")?;

    let tx_hashes = raw
        .get("transactions")
        .and_then(Value::as_array)
        .ok_or(FetchError::MalformedResponse {
            field: "transactions",
        })?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or(FetchError::MalformedResponse {
                    field: "transactions[]",
                })
                .and_then(|s| {
                    B256::from_str(s).map_err(|_| FetchError::MalformedResponse {
                        field: "transactions[]",
                    })
                })
        })
        .collect::<Result<Vec<B256>>>()?;

    Ok(BlockContext {
        number,
        hash,
        parent_hash,
        coinbase,
        chain_id,
        timestamp,
        base_fee_per_gas,
        gas_limit,
        gas_used,
        tx_hashes,
        block_access_list_hash,
    })
}

/// Fetches block-level metadata, using the local cache when available
/// Order of operations (per spec):
/// 1. Resolve `block_id` to a concrete block number via RPC if it's a tag.
/// 2. Check cache for `block_header.json` at that number.
/// 3. On hit, return the cached value.
/// 4. On miss, fetch from RPC, write atomically to cache, then return.
pub async fn fetch_block_metadata(
    client: &RpcClient,
    cache: &CacheConfig,
    chain_id: u64,
    block_id: BlockId,
) -> Result<BlockContext> {
    let block_number = resolve_block_number(client, &block_id).await?;

    let path = cache.block_header_path(chain_id, block_number);

    match read_json::<BlockContext>(&path) {
        Ok(ctx) => return Ok(ctx),
        Err(CacheError::NotFound(_)) => {} // fall through to RPC fetch
        Err(CacheError::Malformed { .. }) => {} // corrupt file, refetch and overwrite
        Err(e) => return Err(e.into()),    // real IO failure, propagate
    }

    let raw = raw_get_block_by_number(client, &BlockId::Number(block_number)).await?;
    let ctx = parse_block_context(&raw, chain_id)?;

    write_json(&path, &ctx)?;

    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use wiremock::matchers::method as http_method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    #[test]
    fn parses_full_block_with_base_fee() {
        let raw = serde_json::json!({
            "number": "0x64",
            "hash": format!("0x{}", "ab".repeat(32)),
            "parentHash": format!("0x{}", "cd".repeat(32)),
            "miner": format!("0x{}", "11".repeat(20)),
            "timestamp": "0x6123abcd",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0xf4240",
            "baseFeePerGas": "0x3b9aca00",
            "transactions": [format!("0x{}", "ef".repeat(32))]
        });

        let ctx = parse_block_context(&raw, 1).unwrap();
        assert_eq!(ctx.number, 100);
        assert_eq!(ctx.chain_id, 1);
        assert!(ctx.base_fee_per_gas.is_some());
        assert_eq!(ctx.tx_hashes.len(), 1);
    }

    #[test]
    fn missing_base_fee_is_none_not_error() {
        let raw = serde_json::json!({
            "number": "0x64",
            "hash": format!("0x{}", "ab".repeat(32)),
            "parentHash": format!("0x{}", "cd".repeat(32)),
            "miner": format!("0x{}", "11".repeat(20)),
            "timestamp": "0x1",
            "gasLimit": "0x1",
            "gasUsed": "0x1",
            "transactions": []
        });

        let ctx = parse_block_context(&raw, 1).unwrap();
        assert!(ctx.base_fee_per_gas.is_none());
    }

    /// A minimal but complete block, so BAL-hash cases only vary the one field.
    fn block_json_with(bal_hash: Option<serde_json::Value>) -> serde_json::Value {
        let mut raw = serde_json::json!({
            "number": "0x64",
            "hash": format!("0x{}", "ab".repeat(32)),
            "parentHash": format!("0x{}", "cd".repeat(32)),
            "miner": format!("0x{}", "11".repeat(20)),
            "timestamp": "0x1",
            "gasLimit": "0x1",
            "gasUsed": "0x1",
            "transactions": []
        });
        if let Some(v) = bal_hash {
            raw["blockAccessListHash"] = v;
        }
        raw
    }

    #[test]
    fn parses_block_access_list_hash_when_present() {
        // the EIP-7928 empty-BAL sentinel, as a Glamsterdam block would report it
        let sentinel = "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347";
        let raw = block_json_with(Some(serde_json::json!(sentinel)));

        let ctx = parse_block_context(&raw, 1).unwrap();

        assert_eq!(
            ctx.block_access_list_hash,
            Some(B256::from_str(sentinel).unwrap())
        );
    }

    // Pre-Glamsterdam blocks have no such field, and some clients emit the key
    // with an explicit null rather than omitting it. Both must parse to `None`
    // rather than failing the way a missing `number` does — they share a match
    // arm, so they share a test.
    #[test]
    fn absent_block_access_list_hash_is_none_not_error() {
        for raw in [block_json_with(None), block_json_with(Some(Value::Null))] {
            let ctx = parse_block_context(&raw, 1).unwrap();
            assert!(ctx.block_access_list_hash.is_none());
        }
    }

    // Present but unparseable is a real malformed response, not an absent field.
    #[test]
    fn malformed_block_access_list_hash_errors_with_field_name() {
        let raw = block_json_with(Some(serde_json::json!("0xnothex")));

        let err = parse_block_context(&raw, 1).unwrap_err();

        assert!(matches!(
            err,
            FetchError::MalformedResponse {
                field: "blockAccessListHash"
            }
        ));
    }

    #[test]
    fn missing_required_field_errors_with_field_name() {
        let raw = serde_json::json!({
            "hash": format!("0x{}", "ab".repeat(32)),
        });

        let err = parse_block_context(&raw, 1).unwrap_err();
        assert!(matches!(
            err,
            FetchError::MalformedResponse { field: "number" }
        ));
    }

    fn sample_block_json(number: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "hash": format!("0x{}", "ab".repeat(32)),
            "parentHash": format!("0x{}", "cd".repeat(32)),
            "miner": format!("0x{}", "11".repeat(20)),
            "timestamp": "0x1",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0xf4240",
            "baseFeePerGas": "0x3b9aca00",
            "transactions": [format!("0x{}", "ef".repeat(32))]
        })
    }

    fn rpc_ok_body(req: &Request, result: serde_json::Value) -> serde_json::Value {
        let id = serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::json!(0));
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    #[tokio::test]
    async fn cache_miss_fetches_via_rpc_and_writes_cache() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(move |req: &Request| {
                ResponseTemplate::new(200)
                    .set_body_json(rpc_ok_body(req, sample_block_json("0x64")))
            })
            .expect(1)
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri()).unwrap();
        let dir = tempdir().unwrap();
        let cache = CacheConfig::with_root(dir.path().to_path_buf());

        let ctx = fetch_block_metadata(&client, &cache, 1, BlockId::Number(100))
            .await
            .unwrap();

        assert_eq!(ctx.number, 100);
        assert!(cache.block_header_path(1, 100).exists());
    }

    #[tokio::test]
    async fn cache_hit_skips_rpc_entirely() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(move |req: &Request| {
                ResponseTemplate::new(200)
                    .set_body_json(rpc_ok_body(req, sample_block_json("0x64")))
            })
            .expect(1)
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri()).unwrap();
        let dir = tempdir().unwrap();
        let cache = CacheConfig::with_root(dir.path().to_path_buf());

        fetch_block_metadata(&client, &cache, 1, BlockId::Number(100))
            .await
            .unwrap();

        let ctx = fetch_block_metadata(&client, &cache, 1, BlockId::Number(100))
            .await
            .unwrap();

        assert_eq!(ctx.number, 100);
    }

    #[tokio::test]
    async fn malformed_cache_file_triggers_refetch_and_overwrite() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(move |req: &Request| {
                ResponseTemplate::new(200)
                    .set_body_json(rpc_ok_body(req, sample_block_json("0x64")))
            })
            .expect(1)
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri()).unwrap();
        let dir = tempdir().unwrap();
        let cache = CacheConfig::with_root(dir.path().to_path_buf());

        let path = cache.block_header_path(1, 100);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not valid json").unwrap();

        let ctx = fetch_block_metadata(&client, &cache, 1, BlockId::Number(100))
            .await
            .unwrap();

        assert_eq!(ctx.number, 100);
        let refreshed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(refreshed["number"], 100);
    }

    #[tokio::test]
    async fn tag_resolves_via_rpc_before_fetching() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(move |req: &Request| {
                ResponseTemplate::new(200)
                    .set_body_json(rpc_ok_body(req, sample_block_json("0x64")))
            })
            .expect(2) // one call to resolve "latest" -> number, one to fetch by number
            .mount(&server)
            .await;

        let client = RpcClient::new(server.uri()).unwrap();
        let dir = tempdir().unwrap();
        let cache = CacheConfig::with_root(dir.path().to_path_buf());

        let ctx = fetch_block_metadata(&client, &cache, 1, BlockId::Tag("latest".into()))
            .await
            .unwrap();

        assert_eq!(ctx.number, 100);
        assert!(cache.block_header_path(1, 100).exists());
    }
}
