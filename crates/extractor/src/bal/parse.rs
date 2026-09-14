use std::collections::HashSet;

use crate::bal::BlockAccessIndexRole::{self, PostExecution};
use alloy_eip7928::{total_bal_items, AccountChanges, BlockAccessIndex, ITEM_COST};
use alloy_primitives::{Address, B256, U256};
use types::types::{
    BlockAccess, BlockContext, StateKey,
    TxPosition::{self, PostTransaction, PreTransaction, Transaction},
    WriteEntry, WriteValue,
};

use crate::bal::{classify_block_access_index, BalError, BlockAccessIndexRole::PreExecution};

// how big a bal is allowed to get scales with the block gas limit instead of a
// flat cap the cost per item sits a little under what a cold storage read
// charges so there is room left for system writes that pay no gas
fn check_size_of_bal(bal: &[AccountChanges], ctx: &BlockContext) -> Result<(), BalError> {
    let gas_limit = ctx.gas_limit;
    let max_items = gas_limit / ITEM_COST as u64;
    // an account counts once and so does every slot it names, the same slot read
    // and written only counts the once
    let item_count = total_bal_items(bal);

    if item_count > max_items {
        return Err(BalError::BlockAccessListTooLarge {
            item_count,
            max_items,
            gas_limit,
        });
    }

    Ok(())
}

// the spec keeps this index inside a uint32 but the type we decode into is a
// plain u64 so a client can hand us anything worth catching here before we try
// to work out which transaction it belongs to
fn check_index_fits_uint32(index: BlockAccessIndex) -> Result<(), BalError> {
    if index > u32::MAX as u64 {
        return Err(BalError::IndexTooLarge { index });
    }

    Ok(())
}

// a slot that gets written is only ever recorded as a write so finding one in
// both lists means the client built something the spec says cannot happen we
// reject it rather than quietly dropping the read and keeping the write
fn check_disjointness_of_changes_and_reads(bal: &[AccountChanges]) -> Result<(), BalError> {
    for account in bal {
        // the set stays per account since the same slot number on two addresses
        // is perfectly normal
        let written: HashSet<U256> = account
            .storage_changes
            .iter()
            .map(|slot_changes| slot_changes.slot)
            .collect();

        for slot in &account.storage_reads {
            if written.contains(slot) {
                return Err(BalError::DisjointnessViolation {
                    address: account.address,
                    slot: *slot,
                });
            }
        }
    }
    Ok(())
}

fn block_access_index_into_tx_position(
    index: BlockAccessIndex,
    tx_count: usize,
) -> Result<TxPosition, BalError> {
    check_index_fits_uint32(index)?;
    match classify_block_access_index(index, tx_count) {
        Ok(PreExecution) => Ok(PreTransaction),
        Ok(BlockAccessIndexRole::Transaction { tx_index }) => Ok(Transaction(tx_index)),
        Ok(PostExecution) => Ok(PostTransaction),
        Err(err) => Err(err),
    }
}

// alloy speaks u256 for slots but our state key wants b256
fn slot_key(address: Address, slot: U256) -> StateKey {
    StateKey::StorageSlot {
        address,
        slot: B256::from(slot.to_be_bytes::<32>()),
    }
}

/// Turns a block access list into a [`BlockAccess`].
///
/// `ctx` supplies what the access list itself cannot: a BAL is keyed by address
/// and carries no transaction count of its own, so how many transactions the
/// block holds comes from the block header.
///
/// Every write keeps both the value written and the position it was written at,
/// so the pre-execution system call at index 0 and the post-execution one at
/// index n+1 stay distinguishable from the transactions that ran between them.
///
/// A malformed BAL is rejected rather than repaired. Reads and writes that
/// overlap on one account, an index that does not fit a uint32, and a list
/// holding more items than the block's gas limit pays for are all errors, and
/// none of them leave a half-built [`BlockAccess`] behind.
///
/// `touched` holds accounts that turned up in the BAL with no writes *and* no
/// reads — they appeared carrying nothing at all. An account with reads has
/// already announced itself through the read set, so it is left out of
/// `touched` rather than counted in both places.
///
/// # The read set is an upper bound
///
/// `reads` cannot be treated as a count of genuine `SLOAD`s. A slot lands in a
/// BAL's `storage_reads` for three different reasons: someone actually read it,
/// someone wrote back the value it already held (a no-op write), or someone
/// wrote it inside a call that later reverted. The encoding records all three
/// identically, so the BAL cannot tell them apart and neither can this parser.
/// Anything counting reads downstream is counting an upper bound on the real
/// number.
pub fn parse_bal(bal: &[AccountChanges], ctx: &BlockContext) -> Result<BlockAccess, BalError> {
    let tx_count = ctx.tx_hashes.len();
    check_size_of_bal(bal, ctx)?;
    check_disjointness_of_changes_and_reads(bal)?;

    let mut writes: Vec<WriteEntry> = Vec::new();
    let mut reads: HashSet<StateKey> = HashSet::new();
    let mut touched: HashSet<Address> = HashSet::new();

    for account in bal {
        let address = account.address;

        for slot_changes in &account.storage_changes {
            let key = slot_key(address, slot_changes.slot);
            for change in &slot_changes.changes {
                let position =
                    block_access_index_into_tx_position(change.block_access_index, tx_count)?;
                writes.push(WriteEntry {
                    position,
                    key: key.clone(),
                    value: WriteValue::Storage(B256::from(change.new_value.to_be_bytes::<32>())),
                });
            }
        }

        for change in &account.balance_changes {
            let position =
                block_access_index_into_tx_position(change.block_access_index, tx_count)?;
            writes.push(WriteEntry {
                position,
                key: StateKey::Balance(address),
                value: WriteValue::Balance(change.post_balance),
            });
        }

        for change in &account.nonce_changes {
            let position =
                block_access_index_into_tx_position(change.block_access_index, tx_count)?;
            writes.push(WriteEntry {
                position,
                key: StateKey::Nonce(address),
                value: WriteValue::Nonce(change.new_nonce),
            });
        }

        for change in &account.code_changes {
            let position =
                block_access_index_into_tx_position(change.block_access_index, tx_count)?;
            writes.push(WriteEntry {
                position,
                key: StateKey::Code(address),
                value: WriteValue::Code(change.new_code.clone()),
            });
        }

        // reads carry no block access index, so block level is as fine as the
        // attribution gets, and there is nothing finer to reconstruct
        //
        // this set is an upper bound on genuine reads rather than a count of
        // them, because a slot arrives here three ways that the encoding cannot
        // tell apart: a real sload, a write of the value already stored, or a
        // write inside a call that later reverted
        for slot in &account.storage_reads {
            reads.insert(slot_key(address, *slot));
        }

        // showed up carrying nothing at all
        if account.storage_changes.is_empty()
            && account.balance_changes.is_empty()
            && account.nonce_changes.is_empty()
            && account.code_changes.is_empty()
            && account.storage_reads.is_empty()
        {
            touched.insert(address);
        }
    }

    Ok(BlockAccess::new(ctx.clone(), writes, reads, touched))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rlp::{Decodable, Encodable};
    use alloy_eip7928::bal::Bal;
    use alloy_primitives::Bytes;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn slot_value(value: u64) -> B256 {
        B256::from(U256::from(value).to_be_bytes::<32>())
    }

    fn ctx_with_txs(tx_count: usize) -> BlockContext {
        ctx_with_txs_and_gas(tx_count, 30_000_000)
    }

    // the size bound is gas based, so the cheapest way to build an over-sized
    // fixture is to shrink the allowance rather than grow the bal
    fn ctx_with_txs_and_gas(tx_count: usize, gas_limit: u64) -> BlockContext {
        BlockContext {
            number: 100,
            hash: B256::from([0xaa; 32]),
            parent_hash: B256::from([0xbb; 32]),
            coinbase: addr(0xcc),
            chain_id: 7082904758,
            timestamp: 1,
            base_fee_per_gas: Some(7),
            gas_limit,
            gas_used: 1,
            tx_hashes: (0..tx_count).map(|i| B256::from([i as u8; 32])).collect(),
            block_access_list_hash: None,
        }
    }

    fn account(raw: serde_json::Value) -> AccountChanges {
        serde_json::from_value(raw).unwrap()
    }

    fn empty_account(address: &str) -> AccountChanges {
        account(serde_json::json!({
            "address": address,
            "storageChanges": [],
            "storageReads": [],
            "balanceChanges": [],
            "nonceChanges": [],
            "codeChanges": []
        }))
    }

    #[test]
    fn two_transaction_bal_parses_with_positions_and_values() {
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [
                { "slot": "0x1", "changes": [
                    { "blockAccessIndex": "0x1", "newValue": "0x2a" },
                    { "blockAccessIndex": "0x2", "newValue": "0x2b" }
                ] }
            ],
            "storageReads": ["0x7"],
            "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
            "nonceChanges": [ { "blockAccessIndex": "0x2", "newNonce": "0x5" } ],
            "codeChanges": [ { "blockAccessIndex": "0x2", "newCode": "0x6001" } ]
        }))];

        let out = parse_bal(&bal, &ctx_with_txs(2)).unwrap();

        // each write keeps the position it was recorded at and the value written
        assert_eq!(
            out.writes,
            vec![
                WriteEntry {
                    position: Transaction(0),
                    key: slot_key(addr(0x11), U256::from(1)),
                    value: WriteValue::Storage(slot_value(0x2a)),
                },
                WriteEntry {
                    position: Transaction(1),
                    key: slot_key(addr(0x11), U256::from(1)),
                    value: WriteValue::Storage(slot_value(0x2b)),
                },
                WriteEntry {
                    position: Transaction(0),
                    key: StateKey::Balance(addr(0x11)),
                    value: WriteValue::Balance(U256::from(1_000_000_000_000_000_000u64)),
                },
                WriteEntry {
                    position: Transaction(1),
                    key: StateKey::Nonce(addr(0x11)),
                    value: WriteValue::Nonce(5),
                },
                WriteEntry {
                    position: Transaction(1),
                    key: StateKey::Code(addr(0x11)),
                    value: WriteValue::Code(Bytes::from_static(&[0x60, 0x01])),
                },
            ]
        );

        // the read is block level, so no transaction can claim it
        assert!(out.exact_reads().is_none());
        assert_eq!(out.reads().len(), 1);
        assert!(out.reads().contains(&slot_key(addr(0x11), U256::from(7))));
        assert!(out.touched.is_empty());
    }

    #[test]
    fn slot_in_both_reads_and_changes_is_rejected() {
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [
                { "slot": "0x1", "changes": [ { "blockAccessIndex": "0x1", "newValue": "0x2a" } ] }
            ],
            "storageReads": ["0x1"],
            "balanceChanges": [],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let err = parse_bal(&bal, &ctx_with_txs(1)).unwrap_err();

        assert!(matches!(
            err,
            BalError::DisjointnessViolation { address, slot }
                if address == addr(0x11) && slot == U256::from(1)
        ));
    }

    #[test]
    fn the_same_slot_on_two_addresses_is_not_a_disjointness_violation() {
        let bal = vec![
            account(serde_json::json!({
                "address": "0x1111111111111111111111111111111111111111",
                "storageChanges": [
                    { "slot": "0x1", "changes": [ { "blockAccessIndex": "0x1", "newValue": "0x2a" } ] }
                ],
                "storageReads": [],
                "balanceChanges": [],
                "nonceChanges": [],
                "codeChanges": []
            })),
            account(serde_json::json!({
                "address": "0x2222222222222222222222222222222222222222",
                "storageChanges": [],
                "storageReads": ["0x1"],
                "balanceChanges": [],
                "nonceChanges": [],
                "codeChanges": []
            })),
        ];

        let out = parse_bal(&bal, &ctx_with_txs(1)).unwrap();

        assert_eq!(out.writes.len(), 1);
        assert!(out.reads().contains(&slot_key(addr(0x22), U256::from(1))));
    }

    #[test]
    fn system_writes_land_outside_the_transactions() {
        // in a 2-tx block index 0 is pre-execution and index 3 is post-execution
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

        let out = parse_bal(&bal, &ctx_with_txs(2)).unwrap();

        let pre: Vec<_> = out.pre_writes().collect();
        let post: Vec<_> = out.post_writes().collect();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].value, WriteValue::Storage(slot_value(0x2a)));
        assert_eq!(post.len(), 1);
        assert_eq!(post[0].value, WriteValue::Storage(slot_value(0x2b)));
        // neither is attributable to a transaction
        assert_eq!(out.tx_writes().count(), 0);
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

        let err = parse_bal(&bal, &ctx_with_txs(2)).unwrap_err();

        assert!(matches!(
            err,
            BalError::InvalidBlockAccessIndex {
                index: 9,
                tx_count: 2
            }
        ));
    }

    #[test]
    fn index_above_uint32_max_gets_its_own_error() {
        // 2^40, far past both the uint32 bound and the block's transaction count
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [],
            "storageReads": [],
            "balanceChanges": [ { "blockAccessIndex": "0x10000000000", "postBalance": "0x1" } ],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let err = parse_bal(&bal, &ctx_with_txs(2)).unwrap_err();

        assert!(matches!(
            err,
            BalError::IndexTooLarge {
                index: 0x100_0000_0000
            }
        ));
    }

    #[test]
    fn bal_larger_than_the_gas_allowance_is_rejected() {
        // 2000 gas buys exactly one item, and two bare accounts are two items
        let bal = vec![
            empty_account("0x1111111111111111111111111111111111111111"),
            empty_account("0x2222222222222222222222222222222222222222"),
        ];

        let err = parse_bal(&bal, &ctx_with_txs_and_gas(1, 2000)).unwrap_err();

        assert!(matches!(
            err,
            BalError::BlockAccessListTooLarge {
                item_count: 2,
                max_items: 1,
                gas_limit: 2000
            }
        ));
    }

    #[test]
    fn a_bal_with_no_reads_parses_with_an_empty_read_set() {
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

        let out = parse_bal(&bal, &ctx_with_txs(1)).unwrap();

        assert_eq!(out.writes.len(), 1);
        assert!(out.reads().is_empty());
    }

    #[test]
    fn an_account_carrying_nothing_is_touched() {
        let bal = vec![empty_account("0x1111111111111111111111111111111111111111")];

        let out = parse_bal(&bal, &ctx_with_txs(1)).unwrap();

        assert_eq!(out.touched.len(), 1);
        assert!(out.touched.contains(&addr(0x11)));
        assert!(out.writes.is_empty());
        assert!(out.reads().is_empty());
    }

    #[test]
    fn an_account_with_only_reads_is_not_touched() {
        // its reads already say it was there, so listing it again would be
        // double bookkeeping
        let bal = vec![account(serde_json::json!({
            "address": "0x1111111111111111111111111111111111111111",
            "storageChanges": [],
            "storageReads": ["0x7"],
            "balanceChanges": [],
            "nonceChanges": [],
            "codeChanges": []
        }))];

        let out = parse_bal(&bal, &ctx_with_txs(1)).unwrap();

        assert!(out.touched.is_empty());
        assert_eq!(out.reads().len(), 1);
    }

    #[test]
    fn a_bal_survives_an_rlp_round_trip_unchanged() {
        let bal = vec![
            account(serde_json::json!({
                "address": "0x1111111111111111111111111111111111111111",
                "storageChanges": [
                    { "slot": "0x1", "changes": [
                        { "blockAccessIndex": "0x1", "newValue": "0x2a" },
                        { "blockAccessIndex": "0x2", "newValue": "0x2b" }
                    ] }
                ],
                "storageReads": ["0x7"],
                "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
                "nonceChanges": [ { "blockAccessIndex": "0x2", "newNonce": "0x5" } ],
                "codeChanges": [ { "blockAccessIndex": "0x2", "newCode": "0x6001" } ]
            })),
            empty_account("0x2222222222222222222222222222222222222222"),
        ];
        let ctx = ctx_with_txs(2);

        let mut encoded = Vec::new();
        Bal::from(bal.clone()).encode(&mut encoded);
        let decoded = Bal::decode(&mut encoded.as_slice()).unwrap();

        assert_eq!(
            parse_bal(&decoded, &ctx).unwrap(),
            parse_bal(&bal, &ctx).unwrap()
        );
    }
}
