use alloy_eip7928::BlockAccessIndex;

use crate::bal::error::BalError;

/// What a `blockAccessIndex` refers to within a block.
///
/// EIP-7928 numbers a block's access indices `0` for pre-execution system
/// calls, `1..=n` for the n transactions in block order, and `n + 1` for
/// post-execution system calls. Only the middle range maps onto a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockAccessIndexRole {
    PreExecution,
    Transaction { tx_index: usize },
    PostExecution,
}

/// Resolves a `blockAccessIndex` against a block holding `tx_count` transactions.
pub fn classify_block_access_index(
    index: BlockAccessIndex,
    tx_count: usize,
) -> Result<BlockAccessIndexRole, BalError> {
    let post_execution = tx_count as u64 + 1;

    if index == 0 {
        Ok(BlockAccessIndexRole::PreExecution)
    } else if index == post_execution {
        Ok(BlockAccessIndexRole::PostExecution)
    } else if index < post_execution {
        Ok(BlockAccessIndexRole::Transaction {
            tx_index: (index - 1) as usize,
        })
    } else {
        Err(BalError::InvalidBlockAccessIndex { index, tx_count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx_index_of(index: u64, tx_count: usize) -> usize {
        match classify_block_access_index(index, tx_count).unwrap() {
            BlockAccessIndexRole::Transaction { tx_index } => tx_index,
            other => panic!("expected a transaction for index {index}, got {other:?}"),
        }
    }

    #[test]
    fn index_zero_is_pre_execution() {
        assert_eq!(
            classify_block_access_index(0, 3).unwrap(),
            BlockAccessIndexRole::PreExecution
        );
    }

    #[test]
    fn transactions_are_offset_by_one() {
        assert_eq!(tx_index_of(1, 3), 0);
        assert_eq!(tx_index_of(2, 3), 1);
        assert_eq!(tx_index_of(3, 3), 2);
    }

    #[test]
    fn index_after_last_transaction_is_post_execution() {
        assert_eq!(
            classify_block_access_index(4, 3).unwrap(),
            BlockAccessIndexRole::PostExecution
        );
    }

    #[test]
    fn index_beyond_post_execution_is_rejected() {
        let err = classify_block_access_index(5, 3).unwrap_err();
        match err {
            BalError::InvalidBlockAccessIndex { index, tx_count } => {
                assert_eq!(index, 5);
                assert_eq!(tx_count, 3);
            }
            other => panic!("expected InvalidBlockAccessIndex, got {other:?}"),
        }
    }

    #[test]
    fn empty_block_has_only_pre_and_post_execution() {
        assert_eq!(
            classify_block_access_index(0, 0).unwrap(),
            BlockAccessIndexRole::PreExecution
        );
        assert_eq!(
            classify_block_access_index(1, 0).unwrap(),
            BlockAccessIndexRole::PostExecution
        );
        assert!(classify_block_access_index(2, 0).is_err());
    }

    #[test]
    fn single_transaction_block_maps_all_three_roles() {
        assert_eq!(
            classify_block_access_index(0, 1).unwrap(),
            BlockAccessIndexRole::PreExecution
        );
        assert_eq!(tx_index_of(1, 1), 0);
        assert_eq!(
            classify_block_access_index(2, 1).unwrap(),
            BlockAccessIndexRole::PostExecution
        );
    }
}
