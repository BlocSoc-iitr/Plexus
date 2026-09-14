pub mod builder;
pub mod errors;

pub use builder::build_graph;

use self::errors::DepGraphError;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use types::types::ConflictType;

#[derive(Debug)]
pub struct DepGraph {
    pub graph: DiGraph<usize, ConflictType>,
    pub tx_count: usize,
    pub block_number: u64,
    node_indices: Vec<NodeIndex>,
}

impl DepGraph {
    pub fn new(tx_count: usize, block_number: u64) -> Self {
        let mut graph = DiGraph::with_capacity(tx_count, 0);
        let node_indices: Vec<NodeIndex> = (0..tx_count).map(|i| graph.add_node(i)).collect();

        Self {
            graph,
            tx_count,
            block_number,
            node_indices,
        }
    }
    /// Return the `NodeIndex` for a given transaction position.
    ///
    /// # Errors
    ///
    /// Returns [`DepGraphError::TxIndexOutOfBounds`] if `tx_index >= self.tx_count`.
    pub fn node_for_tx(&self, tx_index: usize) -> Result<NodeIndex, DepGraphError> {
        self.node_indices
            .get(tx_index)
            .copied()
            .ok_or(DepGraphError::TxIndexOutOfBounds {
                tx_index,
                tx_count: self.tx_count,
            })
    }

    /// Total number of dependency edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }
    /// Returns `true` when the transaction has **no** incoming or outgoing
    /// dependency edges, meaning it can execute independently of every other
    /// transaction in the block.
    ///
    /// # Errors
    ///
    /// Returns [`DepGraphError::TxIndexOutOfBounds`] if `tx_index >= self.tx_count`.
    pub fn is_independent(&self, tx_index: usize) -> Result<bool, DepGraphError> {
        let node = self.node_for_tx(tx_index)?;
        let independent = self
            .graph
            .neighbors_directed(node, Direction::Incoming)
            .next()
            .is_none()
            && self
                .graph
                .neighbors_directed(node, Direction::Outgoing)
                .next()
                .is_none();
        Ok(independent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructible_from_empty_node_set() {
        let g = DepGraph::new(0, 42);
        assert_eq!(g.tx_count, 0);
        assert_eq!(g.edge_count(), 0);
        assert_eq!(g.block_number, 42);
    }

    #[test]
    fn nodes_carry_tx_index_as_weight() {
        let g = DepGraph::new(5, 1);
        for i in 0..5 {
            let node = g.node_for_tx(i).unwrap();
            assert_eq!(g.graph[node], i);
        }
    }

    #[test]
    fn node_with_no_edges_is_independent() {
        let g = DepGraph::new(3, 1);
        assert!(g.is_independent(0).unwrap());
        assert!(g.is_independent(1).unwrap());
        assert!(g.is_independent(2).unwrap());
    }

    #[test]
    fn node_with_edges_is_not_independent() {
        let mut g = DepGraph::new(3, 1);
        let a = g.node_for_tx(0).unwrap();
        let b = g.node_for_tx(1).unwrap();
        g.graph.add_edge(a, b, ConflictType::WriteAfterWrite);
        assert!(!g.is_independent(0).unwrap());
        assert!(!g.is_independent(1).unwrap());
        assert!(g.is_independent(2).unwrap());
    }

    #[test]
    fn out_of_bounds_tx_index_returns_error() {
        let g = DepGraph::new(3, 1);
        assert_eq!(
            g.node_for_tx(3).unwrap_err(),
            DepGraphError::TxIndexOutOfBounds {
                tx_index: 3,
                tx_count: 3
            }
        );
        assert_eq!(
            g.is_independent(10).unwrap_err(),
            DepGraphError::TxIndexOutOfBounds {
                tx_index: 10,
                tx_count: 3
            }
        );
    }
}
