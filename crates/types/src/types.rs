// Core types shared across every module in Plexus.

use std::collections::HashSet;

use alloy_primitives::{Address, B256};
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

// ─── Read Attribution ─────────────────────────────────────────────────────────

// Whether a transaction's read set is exactly attributed or only block-level.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadAttribution {
    /// Exact reads, as produced by `prestateTracer` in trace mode.
    PerTransaction(HashSet<StateKey>),
    /// Block-level reads with no per-transaction attribution, as produced by BAL mode.
    BlockLevel(HashSet<StateKey>),
}

impl ReadAttribution {
    /// Returns the underlying key set regardless of attribution level.
    pub fn keys(&self) -> &HashSet<StateKey> {
        match self {
            ReadAttribution::PerTransaction(k) | ReadAttribution::BlockLevel(k) => k,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

// ─── Block Context ───────────────────────────────────────────────────────────

/// Block-level metadata extracted from the block header.
/// Fields like coinbase can be used later for excluding from conflict detection.

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        assert!(!ReadAttribution::BlockLevel(HashSet::new()).is_exact());
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
            reads: ReadAttribution::BlockLevel(HashSet::new()),
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
}
