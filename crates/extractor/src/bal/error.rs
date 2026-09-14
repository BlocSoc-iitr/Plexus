use alloy_primitives::{Address, B256, U256};
use thiserror::Error;

use crate::cache::CacheError;
use crate::fetcher::{BlockId, FetchError};
use crate::rpc::RpcError;

#[derive(Error, Debug)]
pub enum BalError {
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcError),

    #[error("cache error: {0}")]
    Cache(#[from] CacheError),

    #[error("block not found: {0:?}")]
    BlockNotFound(BlockId),

    #[error("malformed bal response: missing or invalid field '{field}'")]
    MalformedResponse { field: &'static str },

    #[error("invalid hex in raw bal response: {0}")]
    BalHex(#[from] hex::FromHexError),

    #[error("failed to rlp-decode raw bal: {0}")]
    BalRlp(#[from] alloy_rlp::Error),

    /// EIP-7928 assigns index 0 to pre-execution system calls, 1..=n to the n
    /// transactions in block order, and n+1 to post-execution system calls.
    /// Anything above n+1 cannot be attributed and means the client and the
    /// block header disagree about how many transactions the block holds.
    #[error("block access index {index} out of range for a block with {tx_count} transactions")]
    InvalidBlockAccessIndex { index: u64, tx_count: usize },

    #[error("unsupported client '{name}', expected one of: reth, nethermind")]
    UnsupportedClient { name: String },

    /// The fetched BAL does not hash to the `blockAccessListHash` the block
    /// header commits to, so it is not the access list for that block. A bad
    /// fetch, a corrupt cache entry, or a client bug all land here, and none of
    /// them are safe to pass downstream.
    #[error("block access list hash mismatch: computed {computed}, header commits to {expected}")]
    BalHashMismatch { computed: B256, expected: B256 },

    #[error("malformed bal: slot {slot} on address {address} appears in both storage_reads and storage_changes")]
    DisjointnessViolation { address: Address, slot: U256 },

    #[error("index {index} not fitting in a uint32 as per EIP")]
    IndexTooLarge { index: u64 },

    #[error(
        "BAL with item count being {item_count} is too large, max items - {max_items}, gas limit being {gas_limit}"
    )]
    BlockAccessListTooLarge {
        item_count: u64,
        max_items: u64,
        gas_limit: u64,
    },
}

/// Resolving a block tag to a number during a cached BAL fetch goes through the
/// block fetcher, which speaks [`FetchError`]. Its variants are a subset of
/// `BalError`'s, so they flatten one-to-one rather than nesting a fetch error
/// inside a BAL error.
impl From<FetchError> for BalError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Rpc(e) => BalError::Rpc(e),
            FetchError::Cache(e) => BalError::Cache(e),
            FetchError::BlockNotFound(id) => BalError::BlockNotFound(id),
            FetchError::MalformedResponse { field } => BalError::MalformedResponse { field },
        }
    }
}
