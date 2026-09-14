use thiserror::Error;

/// Errors that can occur when operating on a `DepGraph`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DepGraphError {
    #[error("transaction index {tx_index} is out of bounds (tx_count = {tx_count})")]
    TxIndexOutOfBounds { tx_index: usize, tx_count: usize },
    /// The number of access sets does not match the number of transactions
    /// recorded in the block header. Building a graph from mismatched inputs
    /// would silently drop or invent transactions, so we refuse.
    #[error(
        "access set count ({access_set_count}) does not match block tx count ({block_tx_count}) \
         for block {block_number}"
    )]
    TxCountMismatch {
        access_set_count: usize,
        block_tx_count: usize,
        block_number: u64,
    },

    /// An `AccessSet` claims a `tx_index` that disagrees with its position in
    /// the input slice. The builder relies on `access_sets[i].tx_index == i`
    /// so that edge direction (lower index → higher index) matches block order.
    #[error(
        "access set at position {position} carries tx_index {tx_index}; \
         inputs must be sorted by tx_index with no gaps"
    )]
    TxIndexMismatch { position: usize, tx_index: usize },
}
