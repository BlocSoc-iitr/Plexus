use std::collections::HashSet;
use std::sync::Arc;

use alloy_eip7928::{AccountChanges, BlockAccessIndex};
use alloy_primitives::{Address, B256, U256};
use types::types::{AccessSet, BlockContext, ReadAttribution, StateKey};

use crate::bal::error::BalError;
use crate::bal::index::{classify_block_access_index, BlockAccessIndexRole};

/// A block's access list, normalized against the block's transaction order.
///
/// Pre- and post-execution system calls are kept apart from the transactions:
/// they are real state changes, but they belong to no transaction and so cannot
/// take part in intra-block dependency analysis.
#[derive(Debug, Clone)]
pub struct BlockAccessSets {
    pub txs: Vec<AccessSet>,
    pub system_pre: HashSet<StateKey>,
    pub system_post: HashSet<StateKey>,
}

#[derive(Default)]
struct Writes {
    txs: Vec<HashSet<StateKey>>,
    pre: HashSet<StateKey>,
    post: HashSet<StateKey>,
}

impl Writes {
    fn with_tx_count(tx_count: usize) -> Self {
        Self {
            txs: vec![HashSet::new(); tx_count],
            ..Default::default()
        }
    }

    fn insert(
        &mut self,
        index: BlockAccessIndex,
        tx_count: usize,
        key: StateKey,
    ) -> Result<(), BalError> {
        match classify_block_access_index(index, tx_count)? {
            BlockAccessIndexRole::PreExecution => self.pre.insert(key),
            BlockAccessIndexRole::PostExecution => self.post.insert(key),
            BlockAccessIndexRole::Transaction { tx_index } => self.txs[tx_index].insert(key),
        };
        Ok(())
    }
}

fn slot_key(address: Address, slot: U256) -> StateKey {
    StateKey::StorageSlot {
        address,
        slot: B256::from(slot.to_be_bytes::<32>()),
    }
}

/// Turns a block access list into one [`AccessSet`] per transaction.
///
/// `ctx` supplies what the access list itself cannot: a BAL is keyed by address
/// and carries no transaction hashes, so both the `tx_index` to `tx_hash`
/// mapping and the transaction count come from the block header.
pub fn normalize_bal(
    bal: &[AccountChanges],
    ctx: &BlockContext,
) -> Result<BlockAccessSets, BalError> {
    let tx_count = ctx.tx_hashes.len();
    let mut writes = Writes::with_tx_count(tx_count);
    let mut reads = HashSet::new();

    for account in bal {
        let address = account.address;

        for slot_changes in &account.storage_changes {
            let key = slot_key(address, slot_changes.slot);
            for change in &slot_changes.changes {
                writes.insert(change.block_access_index, tx_count, key.clone())?;
            }
        }

        for change in &account.balance_changes {
            writes.insert(
                change.block_access_index,
                tx_count,
                StateKey::Balance(address),
            )?;
        }

        for change in &account.nonce_changes {
            writes.insert(
                change.block_access_index,
                tx_count,
                StateKey::Nonce(address),
            )?;
        }

        for change in &account.code_changes {
            writes.insert(change.block_access_index, tx_count, StateKey::Code(address))?;
        }

        // EIP-7928 records reads per account with no index, so there is nothing
        // finer than block-level to attribute them to
        for slot in &account.storage_reads {
            reads.insert(slot_key(address, *slot));
        }
    }

    let reads = Arc::new(reads);
    let txs = writes
        .txs
        .into_iter()
        .enumerate()
        .map(|(tx_index, writes)| AccessSet {
            tx_index,
            tx_hash: ctx.tx_hashes[tx_index],
            reads: ReadAttribution::BlockLevel(Arc::clone(&reads)),
            writes,
        })
        .collect();

    Ok(BlockAccessSets {
        txs,
        system_pre: writes.pre,
        system_post: writes.post,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn ctx_with_txs(tx_count: usize) -> BlockContext {
        BlockContext {
            number: 100,
            hash: B256::from([0xaa; 32]),
            parent_hash: B256::from([0xbb; 32]),
            coinbase: addr(0xcc),
            chain_id: 7082904758,
            timestamp: 1,
            base_fee_per_gas: Some(7),
            gas_limit: 30_000_000,
            gas_used: 1,
            tx_hashes: (0..tx_count).map(|i| B256::from([i as u8; 32])).collect(),
            block_access_list_hash: None,
        }
    }

    fn account(raw: serde_json::Value) -> AccountChanges {
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn writes_land_on_the_transaction_that_made_them() {
        // blockAccessIndex 1 and 2 are the block's first and second transactions
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [
                { "slot": "0x1", "changes": [
                    { "blockAccessIndex": "0x1", "newValue": "0x2a" },
                    { "blockAccessIndex": "0x2", "newValue": "0x2b" }
                ] }
            ],
            "storageReads": [],
            "balanceChanges": [],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let out = normalize_bal(&bal, &ctx_with_txs(2)).unwrap();

        let key = slot_key(addr(0x11), U256::from(1));
        assert!(out.txs[0].writes.contains(&key));
        assert!(out.txs[1].writes.contains(&key));
        assert_eq!(out.txs[0].tx_index, 0);
        assert_eq!(out.txs[0].tx_hash, B256::from([0u8; 32]));
        assert_eq!(out.txs[1].tx_hash, B256::from([1u8; 32]));
    }

    #[test]
    fn every_write_kind_maps_to_its_state_key() {
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [
                { "slot": "0x1", "changes": [ { "blockAccessIndex": "0x1", "newValue": "0x2a" } ] }
            ],
            "storageReads": [],
            "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
            "nonceChanges": [ { "blockAccessIndex": "0x1", "newNonce": "0x5" } ],
            "codeChanges": [ { "blockAccessIndex": "0x1", "newCode": "0x6001" } ]
        }))];

        let out = normalize_bal(&bal, &ctx_with_txs(1)).unwrap();

        let writes = &out.txs[0].writes;
        assert_eq!(writes.len(), 4);
        assert!(writes.contains(&slot_key(addr(0x11), U256::from(1))));
        assert!(writes.contains(&StateKey::Balance(addr(0x11))));
        assert!(writes.contains(&StateKey::Nonce(addr(0x11))));
        assert!(writes.contains(&StateKey::Code(addr(0x11))));
    }

    #[test]
    fn reads_are_block_level_and_shared_by_every_transaction() {
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [],
            "storageReads": ["0x7", "0x8"],
            "balanceChanges": [],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let out = normalize_bal(&bal, &ctx_with_txs(2)).unwrap();

        for tx in &out.txs {
            assert!(tx.exact_reads().is_none());
            assert_eq!(tx.reads.keys().len(), 2);
            assert!(tx
                .reads
                .keys()
                .contains(&slot_key(addr(0x11), U256::from(7))));
        }
    }

    #[test]
    fn system_writes_are_kept_out_of_the_transactions() {
        // index 0 is pre-execution and index 3 is post-execution in a 2-tx block
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [
                { "slot": "0x1", "changes": [
                    { "blockAccessIndex": "0x0", "newValue": "0x2a" },
                    { "blockAccessIndex": "0x3", "newValue": "0x2b" }
                ] }
            ],
            "storageReads": [],
            "balanceChanges": [],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let out = normalize_bal(&bal, &ctx_with_txs(2)).unwrap();

        let key = slot_key(addr(0x11), U256::from(1));
        assert_eq!(out.txs.len(), 2);
        assert!(out.txs.iter().all(|tx| tx.writes.is_empty()));
        assert!(out.system_pre.contains(&key));
        assert!(out.system_post.contains(&key));
    }

    #[test]
    fn transactions_absent_from_the_bal_still_get_an_access_set() {
        let out = normalize_bal(&[], &ctx_with_txs(3)).unwrap();

        assert_eq!(out.txs.len(), 3);
        for (i, tx) in out.txs.iter().enumerate() {
            assert_eq!(tx.tx_index, i);
            assert!(tx.is_empty());
        }
    }

    #[test]
    fn index_past_post_execution_is_rejected() {
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [],
            "storageReads": [],
            "balanceChanges": [ { "blockAccessIndex": "0x9", "postBalance": "0x1" } ],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let err = normalize_bal(&bal, &ctx_with_txs(2)).unwrap_err();

        assert!(matches!(
            err,
            BalError::InvalidBlockAccessIndex {
                index: 9,
                tx_count: 2
            }
        ));
    }

    #[test]
    fn empty_block_keeps_only_system_writes() {
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [
                { "slot": "0x1", "changes": [ { "blockAccessIndex": "0x1", "newValue": "0x2a" } ] }
            ],
            "storageReads": [],
            "balanceChanges": [],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let out = normalize_bal(&bal, &ctx_with_txs(0)).unwrap();

        assert!(out.txs.is_empty());
        assert!(out.system_pre.is_empty());
        assert_eq!(out.system_post.len(), 1);
    }
}
