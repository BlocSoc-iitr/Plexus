//! End-to-end BAL tests against a live two-client Kurtosis devnet.
//!
//! Everything else in this crate is covered by fixtures and wiremock. These
//! tests are the only ones that put a real client's bytes through the whole
//! path — fetch, commitment verification, disk cache, normalization — and the
//! only ones that check Reth and Nethermind actually agree.
//!
//! They are `#[ignore]`d, so `cargo test` and CI never touch them and never
//! need a devnet. Bring one up with `devnet/network_params.yaml` (see
//! `devnet/README.md`), then run them explicitly:
//!
//! ```bash
//! PLEXUS_RETH_RPC_URL=$(kurtosis port print plexus-bal-devnet el-1-reth-lighthouse rpc) \
//! PLEXUS_NETHERMIND_RPC_URL=$(kurtosis port print plexus-bal-devnet el-2-nethermind-lighthouse rpc) \
//!   cargo test -p parser --test devnet_bal_e2e -- --ignored --nocapture
//! ```
//!
//! Each test only needs the clients it names, so a Reth-only run can set just
//! `PLEXUS_RETH_RPC_URL` and filter to `reth_end_to_end`.
//!
//! `PLEXUS_BLOCK` pins a block (`0x1f4` or `500`). Left unset, the tests walk
//! back from `latest` for a block that has transactions, because a BAL over an
//! empty block would let most of the assertions below pass vacuously.

use std::env;
use std::fs;

use alloy_eip7928::AccountChanges;
use parser::bal::{normalize_bal, BlockAccessSets, ClientKind};
use parser::cache::config::CacheConfig;
use parser::fetcher::{fetch_bal_cached, fetch_block_metadata, BlockId};
use parser::rpc::client::RpcClient;
use tempfile::TempDir;
use types::types::BlockContext;

const RETH_URL_VAR: &str = "PLEXUS_RETH_RPC_URL";
const NETHERMIND_URL_VAR: &str = "PLEXUS_NETHERMIND_RPC_URL";
const BLOCK_VAR: &str = "PLEXUS_BLOCK";

/// How far back to look for a block with transactions before giving up.
const SCAN_DEPTH: u64 = 64;

/// One connected client, with a cache root of its own.
///
/// The private cache is the point, not an incidental tidiness: `bal.json` is
/// deliberately client-agnostic on disk, so two clients sharing a root would
/// turn the second fetch into a cache hit that never reaches the node, and
/// [`clients_agree_on_the_same_block`] would compare a response against itself.
struct Devnet {
    client: RpcClient,
    cache: CacheConfig,
    chain_id: u64,
    // holds the cache directory open; dropping it deletes the cache
    _cache_dir: TempDir,
}

impl Devnet {
    async fn connect(url_var: &str) -> Self {
        let url = env::var(url_var).unwrap_or_else(|_| {
            panic!("set {url_var} to the node's RPC endpoint — see devnet/README.md")
        });

        let client = RpcClient::new(url.clone())
            .unwrap_or_else(|e| panic!("{url_var}={url} is not a usable RPC endpoint: {e}"));

        // ask the node rather than hard-coding the devnet's id, so a config
        // change to network_id doesn't silently split the cache
        let raw: String = client
            .request("eth_chainId", ())
            .await
            .unwrap_or_else(|e| panic!("eth_chainId failed against {url_var}: {e}"));
        let chain_id = u64::from_str_radix(raw.trim_start_matches("0x"), 16)
            .unwrap_or_else(|e| panic!("eth_chainId returned {raw:?}, which is not hex: {e}"));

        let cache_dir = tempfile::tempdir().expect("could not create a temporary cache directory");
        let cache = CacheConfig::with_root(cache_dir.path().to_path_buf());

        Self {
            client,
            cache,
            chain_id,
            _cache_dir: cache_dir,
        }
    }

    async fn header(&self, block: BlockId) -> BlockContext {
        fetch_block_metadata(&self.client, &self.cache, self.chain_id, block)
            .await
            .expect("failed to fetch block header")
    }
}

/// The block to test against: `PLEXUS_BLOCK` if set, else the newest block
/// carrying transactions.
///
/// An idle devnet mines empty blocks indefinitely. Those still have system
/// writes at the pre- and post-execution indices, but no transactions means no
/// per-transaction access sets and nothing meaningful for the two clients to
/// disagree about, so falling back to `latest` would report a pass that proves
/// almost nothing. Failing loudly is the honest outcome.
async fn test_block(devnet: &Devnet) -> BlockContext {
    if let Ok(raw) = env::var(BLOCK_VAR) {
        let number = parse_block_number(&raw);
        let ctx = devnet.header(BlockId::Number(number)).await;
        assert!(
            !ctx.tx_hashes.is_empty(),
            "{BLOCK_VAR}={raw} resolves to block {number}, which has no transactions"
        );
        return ctx;
    }

    let latest = devnet.header(BlockId::Tag("latest".to_string())).await;
    if !latest.tx_hashes.is_empty() {
        return latest;
    }

    let mut number = latest.number;
    for _ in 0..SCAN_DEPTH {
        if number == 0 {
            break;
        }
        number -= 1;

        let ctx = devnet.header(BlockId::Number(number)).await;
        if !ctx.tx_hashes.is_empty() {
            return ctx;
        }
    }

    panic!(
        "no block with transactions in the {SCAN_DEPTH} blocks below {}. \
         The devnet is idle — send it some traffic, or pin a known block with {BLOCK_VAR}.",
        latest.number
    );
}

/// Accepts either `0x1f4` or `500`, since both spellings are natural to reach
/// for and guessing wrong silently tests the wrong block.
fn parse_block_number(raw: &str) -> u64 {
    let trimmed = raw.trim();
    let parsed = match trimmed.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => trimmed.parse(),
    };
    parsed.unwrap_or_else(|e| panic!("{BLOCK_VAR}={raw:?} is not a block number: {e}"))
}

/// The header's commitment, which every verification in this file hangs off.
fn commitment_of(ctx: &BlockContext) -> alloy_primitives::B256 {
    ctx.block_access_list_hash.unwrap_or_else(|| {
        panic!(
            "block {} carries no blockAccessListHash, so nothing here is actually verified. \
             This is the gloas_fork_epoch trap: the devnet defaults it to a \"never\" sentinel \
             and BAL stays inert on otherwise healthy nodes. See devnet/README.md.",
            ctx.number
        )
    })
}

/// Invariants that hold for any normalized BAL, whichever client served it.
fn assert_normalized(out: &BlockAccessSets, ctx: &BlockContext) {
    assert_eq!(
        out.txs.len(),
        ctx.tx_hashes.len(),
        "normalization produced {} access sets for a block with {} transactions",
        out.txs.len(),
        ctx.tx_hashes.len()
    );

    for (position, tx) in out.txs.iter().enumerate() {
        assert_eq!(tx.tx_index, position, "access sets are out of block order");
        assert_eq!(
            tx.tx_hash, ctx.tx_hashes[position],
            "access set {position} carries the wrong transaction hash"
        );
        // a BAL records reads per account with no index, so there is never
        // anything finer than block-level to attribute them to
        assert!(
            !tx.reads.is_exact(),
            "BAL reads must stay block-level; the dep-graph builder relies on \
             this to know it cannot build WAR edges"
        );
    }
}

fn summarize(label: &str, out: &BlockAccessSets, ctx: &BlockContext) {
    println!(
        "{label}: block {} — {} txs, {} system_pre, {} system_post, {} block-level reads",
        ctx.number,
        out.txs.len(),
        out.system_pre.len(),
        out.system_post.len(),
        out.txs
            .first()
            .map(|tx| tx.reads.keys().len())
            .unwrap_or_default()
    );
}

/// Fetch → verify → cache → normalize, against one real client.
async fn run_end_to_end(kind: ClientKind, url_var: &str) {
    let devnet = Devnet::connect(url_var).await;
    let ctx = test_block(&devnet).await;
    let expected = commitment_of(&ctx);

    println!(
        "{kind}: chain {} block {} — {} txs, commits to {expected}",
        devnet.chain_id,
        ctx.number,
        ctx.tx_hashes.len()
    );

    let block = BlockId::Number(ctx.number);
    let path = devnet.cache.bal_path(devnet.chain_id, ctx.number);
    assert!(
        !path.exists(),
        "cache should start empty at {}",
        path.display()
    );

    // The commitment check runs inside this call, so reaching the next line at
    // all means the client's BAL hashed to what the header committed to. This
    // is the check #17 did by hand once; here it runs on every fetch.
    let bal = fetch_bal_cached(
        &devnet.client,
        &devnet.cache,
        devnet.chain_id,
        kind,
        block.clone(),
    )
    .await
    .unwrap_or_else(|e| panic!("{kind}: BAL fetch failed: {e}"));

    assert!(
        !bal.is_empty(),
        "{kind}: a block with {} transactions produced an empty BAL",
        ctx.tx_hashes.len()
    );

    assert!(
        path.exists(),
        "{kind}: fetch did not write {}",
        path.display()
    );
    let on_disk: Vec<AccountChanges> = serde_json::from_str(
        &fs::read_to_string(&path).expect("could not read the cached bal.json"),
    )
    .expect("cached bal.json did not round-trip through serde");
    assert_eq!(
        on_disk, bal,
        "{kind}: cached bal.json is not what was returned"
    );

    // second call takes the cache-hit branch, including its verification
    let cached = fetch_bal_cached(&devnet.client, &devnet.cache, devnet.chain_id, kind, block)
        .await
        .unwrap_or_else(|e| panic!("{kind}: cached BAL fetch failed: {e}"));
    assert_eq!(
        cached, bal,
        "{kind}: cache hit disagreed with the first fetch"
    );

    let out =
        normalize_bal(&bal, &ctx).unwrap_or_else(|e| panic!("{kind}: normalization failed: {e}"));
    assert_normalized(&out, &ctx);
    summarize(&kind.to_string(), &out, &ctx);
}

#[tokio::test]
#[ignore = "needs a live Reth node; set PLEXUS_RETH_RPC_URL — see devnet/README.md"]
async fn reth_end_to_end() {
    run_end_to_end(ClientKind::Reth, RETH_URL_VAR).await;
}

#[tokio::test]
#[ignore = "needs a live Nethermind node; set PLEXUS_NETHERMIND_RPC_URL — see devnet/README.md"]
async fn nethermind_end_to_end() {
    run_end_to_end(ClientKind::Nethermind, NETHERMIND_URL_VAR).await;
}

/// The two clients encode a BAL completely differently — Reth serves JSON,
/// Nethermind raw RLP — so this is what proves the two decode paths converge on
/// the same data rather than each being self-consistently wrong.
#[tokio::test]
#[ignore = "needs both devnet clients; set PLEXUS_RETH_RPC_URL and PLEXUS_NETHERMIND_RPC_URL"]
async fn clients_agree_on_the_same_block() {
    let reth = Devnet::connect(RETH_URL_VAR).await;
    let nethermind = Devnet::connect(NETHERMIND_URL_VAR).await;

    // two nodes on different networks would fail below as a commitment
    // mismatch, which is a confusing way to learn the URLs are wrong
    assert_eq!(
        reth.chain_id, nethermind.chain_id,
        "the two URLs point at different networks"
    );

    // pick the block on one client, then pin both to that number
    let reth_ctx = test_block(&reth).await;
    let block = BlockId::Number(reth_ctx.number);
    let nethermind_ctx = nethermind.header(block.clone()).await;

    assert_eq!(
        reth_ctx.hash, nethermind_ctx.hash,
        "clients disagree on the hash of block {} — they are not on the same chain",
        reth_ctx.number
    );
    assert_eq!(
        reth_ctx.tx_hashes, nethermind_ctx.tx_hashes,
        "clients disagree on the transactions in block {}",
        reth_ctx.number
    );
    assert_eq!(
        reth_ctx.block_access_list_hash, nethermind_ctx.block_access_list_hash,
        "clients disagree on the BAL commitment for block {}",
        reth_ctx.number
    );
    let expected = commitment_of(&reth_ctx);

    let from_reth = fetch_bal_cached(
        &reth.client,
        &reth.cache,
        reth.chain_id,
        ClientKind::Reth,
        block.clone(),
    )
    .await
    .unwrap_or_else(|e| panic!("reth BAL fetch failed: {e}"));

    let from_nethermind = fetch_bal_cached(
        &nethermind.client,
        &nethermind.cache,
        nethermind.chain_id,
        ClientKind::Nethermind,
        block,
    )
    .await
    .unwrap_or_else(|e| panic!("nethermind BAL fetch failed: {e}"));

    // Both were verified against `expected` on the way in, so in principle
    // equality follows: two RLP encodings that hash alike are the same bytes.
    // Asserting it anyway is what catches a decode path that loses or reorders
    // data on only one side, which the hash check alone would not localize.
    assert_eq!(
        from_reth, from_nethermind,
        "block {} verified against {expected} on both clients, yet decoded differently",
        reth_ctx.number
    );

    let reth_out = normalize_bal(&from_reth, &reth_ctx).expect("normalizing the reth BAL failed");
    let nethermind_out = normalize_bal(&from_nethermind, &nethermind_ctx)
        .expect("normalizing the nethermind BAL failed");

    assert_normalized(&reth_out, &reth_ctx);
    assert_normalized(&nethermind_out, &nethermind_ctx);

    for (a, b) in reth_out.txs.iter().zip(nethermind_out.txs.iter()) {
        assert_eq!(a.tx_hash, b.tx_hash, "tx {} differs", a.tx_index);
        assert_eq!(a.writes, b.writes, "tx {} writes differ", a.tx_index);
        assert_eq!(
            a.reads.keys(),
            b.reads.keys(),
            "tx {} reads differ",
            a.tx_index
        );
    }
    assert_eq!(
        reth_out.system_pre, nethermind_out.system_pre,
        "pre-execution system writes differ"
    );
    assert_eq!(
        reth_out.system_post, nethermind_out.system_post,
        "post-execution system writes differ"
    );

    summarize("reth", &reth_out, &reth_ctx);
    summarize("nethermind", &nethermind_out, &nethermind_ctx);
    println!("both clients agree on block {}", reth_ctx.number);
}
