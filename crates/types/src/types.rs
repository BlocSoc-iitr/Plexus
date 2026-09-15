// Core types shared across every module in Plexus.
use std::collections::HashSet;
use std::sync::Arc;

use alloy_primitives::{Address, Bytes, B256, U256};
use serde::{Deserialize, Serialize};

// ─── State Key ───────────────────────────────────────────────────────────────

// Slot Level Granularity

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StateKey {
    /// An individual storage slot within a contract.
    StorageSlot {
        address: Address,
        slot: B256,
    },
    Balance(Address),
    Nonce(Address),
    Code(Address),
}

impl StateKey {
    pub fn address(&self) -> Address {
        match self {
            StateKey::StorageSlot { address, .. } => *address,
            StateKey::Balance(address) | StateKey::Nonce(address) | StateKey::Code(address) => {
                *address
            }
        }
    }
}

// ─── Read Attribution ─────────────────────────────────────────────────────────

// Whether a transaction's read set is exactly attributed or only block-level.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReadAttribution {
    /// Exact reads, as produced by `prestateTracer` in trace mode.
    PerTransaction(HashSet<StateKey>),
    /// Block-level reads with no per-transaction attribution, as produced by BAL mode.
    BlockLevel(Arc<HashSet<StateKey>>),
}

impl ReadAttribution {
    /// Returns the underlying key set regardless of attribution level.
    pub fn keys(&self) -> &HashSet<StateKey> {
        match self {
            ReadAttribution::PerTransaction(k) => k,
            ReadAttribution::BlockLevel(k) => k,
        }
    }

    /// Returns `true` if reads are exactly attributed to this transaction.
    pub fn is_exact(&self) -> bool {
        matches!(self, ReadAttribution::PerTransaction(_))
    }
}

// ─── Access Set ──────────────────────────────────────────────────────────────

/// Normalized output of both extractors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessSet {
    pub tx_index: usize,
    pub tx_hash: B256,
    pub reads: ReadAttribution,
    pub writes: HashSet<StateKey>,
}

impl AccessSet {
    /// Returns `true` if this transaction touched no state at all.
    pub fn is_empty(&self) -> bool {
        self.reads.keys().is_empty() && self.writes.is_empty()
    }

    /// Returns the exact read set, or None if reads are only block-level.
    ///  graph builder will use this to decide whether to build WAR edges.
    pub fn exact_reads(&self) -> Option<&HashSet<StateKey>> {
        match &self.reads {
            ReadAttribution::PerTransaction(k) => Some(k),
            ReadAttribution::BlockLevel(_) => None,
        }
    }
}

// ─── Conflict Type ───────────────────────────────────────────────────────────

/// The dependency relationship between two transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictType {
    /// Both transactions write the same state key.
    /// The later write must observe the correct post-execution order.
    WriteAfterWrite,
    /// tx_j reads a state key that tx_i writes.
    /// tx_j must observe tx_i's post-execution value, so i → j.
    ReadAfterWrite,
    /// tx_j writes a state key that tx_i reads.
    /// tx_i must observe the pre-execution value, so i → j.
    WriteAfterRead,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WriteValue {
    Storage(B256),
    Balance(U256),
    Nonce(u64),
    Code(Bytes),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TxPosition {
    PreTransaction,
    Transaction(usize),
    PostTransaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WriteEntry {
    pub position: TxPosition,
    pub key: StateKey,
    pub value: WriteValue,
}

// ─── Block Context ───────────────────────────────────────────────────────────

/// Block-level metadata extracted from the block header.
/// Fields like coinbase can be used later for excluding from conflict detection.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockContext {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,

    pub coinbase: Address,
    pub chain_id: u64,
    pub timestamp: u64,
    pub base_fee_per_gas: Option<u128>,
    pub gas_limit: u64,
    pub gas_used: u64,
    /// Ordered transaction hashes. Index in this Vec = `tx_index` in `AccessSet`.
    pub tx_hashes: Vec<B256>,
    /// EIP-7928 commitment to the block's access list: the Keccak-256 of the
    /// RLP-encoded BAL, as reported by the header's `blockAccessListHash`.
    ///
    /// `None` on pre-Glamsterdam blocks and on clients that don't report the
    /// field, so its absence is not an error — it only means the fetched BAL
    /// can't be checked against a commitment.
    ///
    /// Being an `Option` is what keeps `block_header.json` files cached before
    /// this field existed readable: serde deserializes a missing field as `None`
    /// rather than failing to parse and forcing a refetch.
    pub block_access_list_hash: Option<B256>,
}

// ─── Block Access ────────────────────────────────────────────────────────────

/// BAL-shaped view of a block: block-scoped reads, a flat write log carrying
/// system writers alongside txs, and accounts touched with no recorded change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockAccess {
    pub block: BlockContext,
    /// Flat log- system writers appear as PreTx or PostTx (Tx - Transaction),
    /// txs as Transaction(i)
    /// both the post-value and the pre/post sentinels
    pub writes: Vec<WriteEntry>,
    pub reads: ReadAttribution,
    pub touched: HashSet<Address>,
}

impl BlockAccess {
    /// Builds a block access set, forcing reads to block-level attribution
    pub fn new(
        block: BlockContext,
        writes: Vec<WriteEntry>,
        reads: HashSet<StateKey>,
        touched: HashSet<Address>,
    ) -> Self {
        Self {
            block,
            writes,
            reads: ReadAttribution::BlockLevel(Arc::new(reads)),
            touched,
        }
    }

    //narrow by design
    pub fn reads(&self) -> &HashSet<StateKey> {
        self.reads.keys()
    }

    /// mirrors AccessSet::exact_reads, always none for BAL data
    pub fn exact_reads(&self) -> Option<&HashSet<StateKey>> {
        match &self.reads {
            ReadAttribution::PerTransaction(k) => Some(k),
            ReadAttribution::BlockLevel(_) => None,
        }
    }

    /// Writes recorded at one position in the block.
    pub fn writes_for(&self, position: TxPosition) -> impl Iterator<Item = &WriteEntry> {
        self.writes.iter().filter(move |w| w.position == position)
    }

    /// pre-execution system-call writes (BAL index 0)
    pub fn pre_writes(&self) -> impl Iterator<Item = &WriteEntry> {
        self.writes_for(TxPosition::PreTransaction)
    }

    /// post-execution (BAL index n+1)
    pub fn post_writes(&self) -> impl Iterator<Item = &WriteEntry> {
        self.writes_for(TxPosition::PostTransaction)
    }

    /// writes attributable to real transactions, paired with their tx index
    pub fn tx_writes(&self) -> impl Iterator<Item = (usize, &WriteEntry)> {
        self.writes.iter().filter_map(|w| match w.position {
            TxPosition::Transaction(i) => Some((i, w)),
            _ => None,
        })
    }

    /// true if nothing was written, read, or touched.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.reads.keys().is_empty() && self.touched.is_empty()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn slot(byte: u8) -> B256 {
        B256::from([byte; 32])
    }

    #[test]
    fn identical_state_keys_deduplicate_in_hashset() {
        let mut set = HashSet::new();
        set.insert(StateKey::StorageSlot {
            address: addr(1),
            slot: slot(1),
        });
        set.insert(StateKey::StorageSlot {
            address: addr(1),
            slot: slot(1),
        });
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn distinct_slots_same_address_are_independent() {
        // Core correctness guarantee: two USDC transfers to different users
        // must not be flagged as conflicting.
        let usdc = addr(0xA0);
        let mut set = HashSet::new();
        set.insert(StateKey::StorageSlot {
            address: usdc,
            slot: slot(1),
        });
        set.insert(StateKey::StorageSlot {
            address: usdc,
            slot: slot(2),
        });
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn balance_and_storage_slot_for_same_address_are_distinct() {
        let a = addr(1);
        assert_ne!(
            StateKey::Balance(a),
            StateKey::StorageSlot {
                address: a,
                slot: slot(0)
            }
        );
    }

    #[test]
    fn read_attribution_is_exact_iff_per_transaction() {
        assert!(ReadAttribution::PerTransaction(HashSet::new()).is_exact());
        assert!(!ReadAttribution::BlockLevel(Arc::new(HashSet::new())).is_exact());
    }

    #[test]
    fn access_set_is_empty_with_no_reads_or_writes() {
        let a = AccessSet {
            tx_index: 0,
            tx_hash: slot(0),
            reads: ReadAttribution::PerTransaction(HashSet::new()),
            writes: HashSet::new(),
        };
        assert!(a.is_empty());
    }

    #[test]
    fn exact_reads_returns_none_for_block_level_attribution() {
        let a = AccessSet {
            tx_index: 0,
            tx_hash: slot(0),
            reads: ReadAttribution::BlockLevel(Arc::new(HashSet::new())),
            writes: HashSet::new(),
        };
        assert!(a.exact_reads().is_none());
    }

    #[test]
    fn exact_reads_returns_some_for_per_transaction_attribution() {
        let mut reads = HashSet::new();
        reads.insert(StateKey::Balance(addr(1)));
        let a = AccessSet {
            tx_index: 0,
            tx_hash: slot(0),
            reads: ReadAttribution::PerTransaction(reads),
            writes: HashSet::new(),
        };
        assert!(a.exact_reads().is_some());
    }

    // A `block_header.json` cached before `block_access_list_hash` existed must
    // still deserialize, otherwise every existing cache entry reads back as
    // malformed and silently triggers a refetch.
    #[test]
    fn block_context_without_bal_hash_field_still_deserializes() {
        let legacy = serde_json::json!({
            "number": 100,
            "hash": slot(0xab),
            "parent_hash": slot(0xcd),
            "coinbase": addr(0x11),
            "chain_id": 1,
            "timestamp": 1,
            "base_fee_per_gas": null,
            "gas_limit": 30_000_000,
            "gas_used": 1,
            "tx_hashes": []
        });

        let ctx: BlockContext = serde_json::from_value(legacy).unwrap();

        assert_eq!(ctx.number, 100);
        assert!(ctx.block_access_list_hash.is_none());
    }

    fn ctx() -> BlockContext {
        BlockContext {
            number: 21_000_000,
            hash: slot(0xB1),
            parent_hash: slot(0xB0),
            coinbase: addr(0xC0),
            chain_id: 1,
            timestamp: 1_700_000_000,
            base_fee_per_gas: Some(7),
            gas_limit: 30_000_000,
            gas_used: 12_345,
            tx_hashes: vec![slot(0x11), slot(0x22)],
            block_access_list_hash: None,
        }
    }

    // a small block with one write before the txs, two tx writes, one after,
    // plus a block level read and an account that was only touched
    fn sample_block() -> BlockAccess {
        let writes = vec![
            WriteEntry {
                position: TxPosition::PreTransaction,
                key: StateKey::StorageSlot {
                    address: addr(0x02),
                    slot: slot(0x01),
                },
                value: WriteValue::Storage(slot(0xAA)),
            },
            WriteEntry {
                position: TxPosition::Transaction(0),
                key: StateKey::Balance(addr(0x10)),
                value: WriteValue::Balance(U256::from(5u64)),
            },
            WriteEntry {
                position: TxPosition::Transaction(1),
                key: StateKey::Nonce(addr(0x10)),
                value: WriteValue::Nonce(3),
            },
            WriteEntry {
                position: TxPosition::PostTransaction,
                key: StateKey::Code(addr(0x03)),
                value: WriteValue::Code(Bytes::from_static(&[0x60, 0x00])),
            },
        ];

        let mut reads = HashSet::new();
        reads.insert(StateKey::StorageSlot {
            address: addr(0x10),
            slot: slot(0x07),
        });

        let mut touched = HashSet::new();
        touched.insert(addr(0xF1));

        BlockAccess::new(ctx(), writes, reads, touched)
    }

    #[test]
    fn block_access_round_trips() {
        let original = sample_block();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: BlockAccess = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn system_writer_entries_are_distinguishable_from_txs() {
        let b = sample_block();

        // the two system writers stay on their own
        assert_eq!(b.pre_writes().count(), 1);
        assert_eq!(b.post_writes().count(), 1);

        // and they never show up among the real transactions
        let tx: Vec<_> = b.tx_writes().collect();
        assert_eq!(tx.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1]);
        assert!(b
            .tx_writes()
            .all(|(_, w)| matches!(w.position, TxPosition::Transaction(_))));

        // asking for one position gives back only that position
        let at_one: Vec<_> = b.writes_for(TxPosition::Transaction(1)).collect();
        assert_eq!(at_one.len(), 1);
        assert_eq!(at_one[0].key, StateKey::Nonce(addr(0x10)));
        assert_eq!(b.writes_for(TxPosition::Transaction(9)).count(), 0);
    }

    #[test]
    fn block_level_reads_have_no_exact_attribution() {
        let b = sample_block();
        assert!(!b.reads.is_exact());
        assert!(b.exact_reads().is_none());
        assert_eq!(b.reads().len(), 1);
    }

    #[test]
    fn touched_is_separate_from_reads() {
        let b = sample_block();
        let touched_addr = addr(0xF1);
        assert!(b.touched.contains(&touched_addr));

        // a touched account does not turn into a read we never saw
        assert!(!b.reads().contains(&StateKey::Balance(touched_addr)));
        assert!(!b.reads().contains(&StateKey::Nonce(touched_addr)));
        assert!(!b.reads().contains(&StateKey::Code(touched_addr)));
    }

    #[test]
    fn empty_block_access_is_empty() {
        assert!(BlockAccess::new(ctx(), Vec::new(), HashSet::new(), HashSet::new()).is_empty());
        assert!(!sample_block().is_empty());
    }

    #[test]
    fn state_key_address_returns_address_for_every_variant() {
        let address = addr(0x11);

        assert_eq!(
            StateKey::StorageSlot {
                address,
                slot: slot(0x01),
            }
            .address(),
            address
        );

        assert_eq!(StateKey::Balance(address).address(), address);
        assert_eq!(StateKey::Code(address).address(), address);
        assert_eq!(StateKey::Nonce(address).address(), address);
    }
}
