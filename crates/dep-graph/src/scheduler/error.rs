use std::fmt;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    ZeroCores,
    CyclicGraph { tx_index: usize },
    TxIndexOutOfBounds { tx_index: usize, tx_count: usize },
    MissingGasEntry { tx_index: usize, tx_count: usize },
    DuplicateGasEntry { tx_index: usize },
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScheduleError::ZeroCores => {
                write!(f, "core count must be at least 1")
            }
            ScheduleError::CyclicGraph { tx_index } => {
                write!(
                    f,
                    "dependency graph contains a cycle involving tx {tx_index}; \
                     no topological order exists"
                )
            }
            ScheduleError::TxIndexOutOfBounds { tx_index, tx_count } => {
                write!(
                    f,
                    "gas entry references tx index {tx_index} but the graph only \
                     has {tx_count} transactions"
                )
            }
            ScheduleError::MissingGasEntry { tx_index, tx_count } => {
                write!(
                    f,
                    "no gas entry for tx index {tx_index} (the graph has {tx_count} \
                     transactions and each one needs exactly one gas entry)"
                )
            }
            ScheduleError::DuplicateGasEntry { tx_index } => {
                write!(
                    f,
                    "multiple gas entries for tx index {tx_index}; each transaction \
                     needs exactly one gas entry"
                )
            }
        }
    }
}

impl std::error::Error for ScheduleError {}
