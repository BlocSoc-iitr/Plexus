//! Block access list (EIP-7928) fetching and normalization.
//!
//! Response shapes differ per client, but all of them decode into the
//! `alloy_eip7928` types, so only the fetch path is client-specific.

pub mod client_kind;
pub mod commitment;
pub mod error;
pub mod index;
pub mod nethermind;
pub mod normalize;
pub mod parse;
pub mod reth;

pub use client_kind::ClientKind;
pub use commitment::{verify_bal_commitment, verify_raw_bal_commitment};
pub use error::BalError;
pub use index::{classify_block_access_index, BlockAccessIndexRole};
pub use nethermind::decode_raw_bal_verified;
pub use nethermind::fetch_nethermind_bal;
pub use normalize::{normalize_bal, BlockAccessSets};
pub use parse::parse_bal;
pub use reth::fetch_reth_bal;

use alloy_eip7928::AccountChanges;
use alloy_primitives::B256;

use crate::fetcher::BlockId;
use crate::rpc::client::RpcClient;

/// Fetches a block's access list, dispatching to the fetch path for `kind`.
///
/// Reth and Nethermind expose the BAL through different RPC methods and wire
/// encodings, but both decode into the same `Vec<AccountChanges>`, so this is
/// the single client-agnostic entry point callers (and the cache layer in
/// [`fetch_bal_cached`](crate::fetcher::fetch_bal_cached)) use.
///
/// `expected` is the block header's `blockAccessListHash`. When it's `Some`, the
/// response is checked against that commitment and a mismatch is an error, so no
/// caller can use a BAL that isn't the one the block committed to. `None` skips
/// the check, for blocks whose header carries no commitment.
///
/// Both clients are verified, but not equally: Nethermind's raw-RLP path checks
/// the bytes the node sent, while Reth's JSON path can only check a re-encoding
/// of the decoded response. See [`commitment`] for what that distinction costs.
pub async fn fetch_bal(
    client: &RpcClient,
    kind: ClientKind,
    block_id: &BlockId,
    expected: Option<B256>,
) -> Result<Vec<AccountChanges>, BalError> {
    match kind {
        ClientKind::Reth => fetch_reth_bal(client, block_id, expected).await,
        ClientKind::Nethermind => fetch_nethermind_bal(client, block_id, expected).await,
    }
}

#[cfg(test)]
mod tests {
    use alloy_eip7928::{bal::Bal, AccountChanges, EMPTY_BLOCK_ACCESS_LIST_HASH};

    #[test]
    fn reth_response_shape_decodes_into_alloy_types() {
        let raw = serde_json::json!([
            {
                "address": "0x1111111111111111111111111111111111111111",
                "storageChanges": [
                    { "slot": "0x1", "changes": [ { "blockAccessIndex": "0x1", "newValue": "0x2a" } ] }
                ],
                "storageReads": ["0x7"],
                "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
                "nonceChanges": [ { "blockAccessIndex": "0x1", "newNonce": "0x5" } ],
                "codeChanges": []
            }
        ]);

        let bal: Vec<AccountChanges> = serde_json::from_value(raw).unwrap();

        let account = &bal[0];
        assert_eq!(account.storage_changes[0].changes[0].block_access_index, 1);
        assert_eq!(account.storage_reads.len(), 1);
        assert_eq!(account.nonce_changes[0].new_nonce, 5);
    }

    // `alloy-eips` pulls in `alloy-eip7928` without its `rlp` feature, so the
    // hashing and RLP decoding this module needs only exist because of the
    // direct dependency. This fails to compile if that dependency is dropped
    // back to what `alloy` alone provides.
    #[test]
    fn rlp_feature_is_enabled_on_alloy_eip7928() {
        assert_eq!(Bal::default().compute_hash(), EMPTY_BLOCK_ACCESS_LIST_HASH);
    }
}

// End-to-end: the fetch path and the normalizer wired together, driven by a
// wiremock-served Reth response, exercising exactly the two steps a caller runs.
#[cfg(test)]
mod e2e_tests {
    use alloy_primitives::{Address, B256, U256};
    use types::types::{BlockContext, StateKey};
    use wiremock::matchers::method as http_method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::{fetch_reth_bal, normalize_bal};
    use crate::fetcher::BlockId;
    use crate::rpc::client::RpcClient;

    // A realistic Reth response: two accounts, two transactions, and a
    // pre-execution system write (a beacon-root ring-buffer slot at index 0), of
    // the shape a live Glamsterdam block produces. Sourced from the alloy-eip7928
    // serde shape; replace with a node-captured fixture once a Kurtosis devnet
    // with `el_type: reth` is stood up (the public devnet RPC is load-balanced
    // and cannot be pinned to Reth).
    fn reth_block_bal() -> serde_json::Value {
        serde_json::json!([
            {
                "address": "0x000f3df6d732807ef1319fb7b8bb8522d0beac02",
                "storageChanges": [
                    { "slot": "0x2a", "changes": [ { "blockAccessIndex": "0x0", "newValue": "0x99" } ] }
                ],
                "storageReads": [],
                "balanceChanges": [],
                "nonceChanges": [],
                "codeChanges": []
            },
            {
                "address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "storageChanges": [
                    { "slot": "0x1", "changes": [
                        { "blockAccessIndex": "0x1", "newValue": "0x64" },
                        { "blockAccessIndex": "0x2", "newValue": "0xc8" }
                    ] }
                ],
                "storageReads": ["0x5"],
                "balanceChanges": [
                    { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" }
                ],
                "nonceChanges": [
                    { "blockAccessIndex": "0x1", "newNonce": "0x1" }
                ],
                "codeChanges": []
            }
        ])
    }

    fn two_tx_ctx() -> BlockContext {
        BlockContext {
            number: 0x46c5,
            hash: B256::from([0xab; 32]),
            parent_hash: B256::from([0xcd; 32]),
            coinbase: Address::from([0xcc; 20]),
            chain_id: 7082904758,
            timestamp: 0x6a594248,
            base_fee_per_gas: Some(7),
            gas_limit: 30_000_000,
            gas_used: 100_000,
            tx_hashes: vec![B256::from([0x11; 32]), B256::from([0x22; 32])],
            block_access_list_hash: None,
        }
    }

    async fn reth_server(result: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(move |req: &Request| {
                let id = serde_json::from_slice::<serde_json::Value>(&req.body)
                    .ok()
                    .and_then(|v| v.get("id").cloned())
                    .unwrap_or(serde_json::json!(0));
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result.clone() }),
                )
            })
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn fetch_then_normalize_produces_expected_access_sets() {
        let server = reth_server(reth_block_bal()).await;
        let client = RpcClient::new(server.uri()).unwrap();
        let ctx = two_tx_ctx();

        let bal = fetch_reth_bal(&client, &BlockId::Number(ctx.number), None)
            .await
            .unwrap();
        let out = normalize_bal(&bal, &ctx).unwrap();

        // one AccessSet per transaction, hashes aligned with block order
        assert_eq!(out.txs.len(), 2);
        assert_eq!(out.txs[0].tx_hash, B256::from([0x11; 32]));
        assert_eq!(out.txs[1].tx_hash, B256::from([0x22; 32]));

        let account = Address::from([0xaa; 20]);
        let slot1 = StateKey::StorageSlot {
            address: account,
            slot: B256::from(U256::from(1).to_be_bytes::<32>()),
        };

        // tx 0 (index 1) wrote the slot, its balance, and its nonce
        assert!(out.txs[0].writes.contains(&slot1));
        assert!(out.txs[0].writes.contains(&StateKey::Balance(account)));
        assert!(out.txs[0].writes.contains(&StateKey::Nonce(account)));
        // tx 1 (index 2) wrote only the slot
        assert!(out.txs[1].writes.contains(&slot1));
        assert!(!out.txs[1].writes.contains(&StateKey::Balance(account)));

        // the index-0 write is a system write, kept out of both transactions
        let system_slot = StateKey::StorageSlot {
            address: Address::from([
                0x00, 0x0f, 0x3d, 0xf6, 0xd7, 0x32, 0x80, 0x7e, 0xf1, 0x31, 0x9f, 0xb7, 0xb8, 0xbb,
                0x85, 0x22, 0xd0, 0xbe, 0xac, 0x02,
            ]),
            slot: B256::from(U256::from(0x2a).to_be_bytes::<32>()),
        };
        assert!(out.system_pre.contains(&system_slot));
        assert!(out.system_post.is_empty());

        // the sole read is block-level and shared by every transaction
        let read_slot = StateKey::StorageSlot {
            address: account,
            slot: B256::from(U256::from(5).to_be_bytes::<32>()),
        };
        for tx in &out.txs {
            assert!(tx.exact_reads().is_none());
            assert!(tx.reads.keys().contains(&read_slot));
        }
    }
}

// Reth (JSON) and Nethermind (raw RLP) are two encodings of the same BAL. Both
// fetch paths decode into `Vec<AccountChanges>`, so proving they yield identical
// values proves the shared `normalize_bal` produces identical `AccessSet`s.
#[cfg(test)]
mod agreement_tests {
    use alloy_eip7928::{bal::Bal, AccountChanges};

    use super::nethermind::decode_raw_bal_verified;

    fn sample() -> serde_json::Value {
        serde_json::json!([
            {
                "address": "0x000f3df6d732807ef1319fb7b8bb8522d0beac02",
                "storageChanges": [
                    { "slot": "0x2a", "changes": [ { "blockAccessIndex": "0x0", "newValue": "0x99" } ] }
                ],
                "storageReads": [],
                "balanceChanges": [],
                "nonceChanges": [],
                "codeChanges": []
            },
            {
                "address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "storageChanges": [
                    { "slot": "0x1", "changes": [
                        { "blockAccessIndex": "0x1", "newValue": "0x64" },
                        { "blockAccessIndex": "0x2", "newValue": "0xc8" }
                    ] }
                ],
                "storageReads": ["0x5"],
                "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
                "nonceChanges": [ { "blockAccessIndex": "0x1", "newNonce": "0x1" } ],
                "codeChanges": []
            }
        ])
    }

    #[test]
    fn reth_and_nethermind_decode_the_same_underlying_data() {
        let via_reth: Vec<AccountChanges> = serde_json::from_value(sample()).unwrap();

        let rlp = alloy_rlp::encode(Bal::from(via_reth.clone()));
        let raw_hex = format!("0x{}", hex::encode(rlp));
        let via_nethermind = decode_raw_bal_verified(&raw_hex, None).unwrap();

        assert_eq!(via_reth, via_nethermind);
    }
}
