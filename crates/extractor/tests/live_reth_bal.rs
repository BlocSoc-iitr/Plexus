//! Live smoke test against a real Reth node (e.g. a local Kurtosis devnet).
//!
//! Ignored by default so `cargo test` stays offline. Run it explicitly with the
//! node's RPC URL:
//!
//! ```bash
//! PLEXUS_RPC_URL=http://127.0.0.1:PORT \
//!   cargo test -p parser --test live_reth_bal -- --ignored --nocapture
//! ```
//!
//! Optionally pin a block with `PLEXUS_BLOCK=0x1234` (defaults to `latest`).

use alloy_eip7928::compute_block_access_list_hash;
use parser::bal::{fetch_reth_bal, normalize_bal};
use parser::cache::config::CacheConfig;
use parser::fetcher::{fetch_block_metadata, BlockId};
use parser::rpc::client::RpcClient;
use std::env;

#[tokio::test]
#[ignore = "requires a live Reth node; set PLEXUS_RPC_URL"]
async fn live_fetch_and_normalize() {
    let url =
        env::var("PLEXUS_RPC_URL").expect("set PLEXUS_RPC_URL to the Reth node's RPC endpoint");
    let block_id = match env::var("PLEXUS_BLOCK") {
        Ok(hex) => BlockId::Number(u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap()),
        Err(_) => BlockId::Tag("latest".to_string()),
    };

    let client = RpcClient::new(url).unwrap();

    // the chain id keys the cache; ask the node rather than hard-coding a devnet
    let chain_id_hex: String = client.request("eth_chainId", ()).await.unwrap();
    let chain_id = u64::from_str_radix(chain_id_hex.trim_start_matches("0x"), 16).unwrap();
    println!("chain_id = {chain_id}");

    // the header gives us the transaction hashes the BAL itself doesn't carry
    let cache = CacheConfig::with_root(env::temp_dir().join("plexus-live-test"));
    let ctx = fetch_block_metadata(&client, &cache, chain_id, block_id.clone())
        .await
        .expect("failed to fetch block header");
    println!(
        "block {} has {} transactions",
        ctx.number,
        ctx.tx_hashes.len()
    );

    // Passing the header's commitment makes the fetch itself reject a BAL that
    // isn't this block's. The unit tests verify against a commitment we computed
    // ourselves; this is the only place a real node's header hash checks a real
    // node's BAL. A BalHashMismatch here renders both hashes in its message.
    let bal = fetch_reth_bal(
        &client,
        &BlockId::Number(ctx.number),
        ctx.block_access_list_hash,
    )
    .await
    .expect("failed to fetch BAL — a -32601 here means the method name is wrong");
    println!("BAL covers {} accounts", bal.len());

    match ctx.block_access_list_hash {
        Some(expected) => {
            println!("header commits to {expected}");
            println!("BAL hashes to     {}", compute_block_access_list_hash(&bal));
            println!("commitment verified");
        }
        // gloas_fork_epoch is almost certainly unset on the devnet — see the
        // Kurtosis findings in the BAL research notes
        None => println!("header carries no blockAccessListHash; skipping verification"),
    }

    let out = normalize_bal(&bal, &ctx).expect("failed to normalize BAL");

    println!("--- normalized ---");
    println!("transactions:      {}", out.txs.len());
    println!("system_pre writes: {}", out.system_pre.len());
    println!("system_post writes:{}", out.system_post.len());
    for tx in &out.txs {
        println!(
            "  tx {:>3}  writes={:<4} reads={}",
            tx.tx_index,
            tx.writes.len(),
            tx.reads.keys().len()
        );
    }

    // one AccessSet per transaction in the header — the core invariant
    assert_eq!(out.txs.len(), ctx.tx_hashes.len());
}
