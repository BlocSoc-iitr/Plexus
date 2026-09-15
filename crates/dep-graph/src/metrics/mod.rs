//! Metrics over a built [`crate::graph::DepGraph`], such as parallelism and
//! task-group (weakly connected component) statistics.

use crate::graph::DepGraph;
use petgraph::algo::{connected_components, toposort};
use petgraph::Direction;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockMetrics {
    pub tx_count: usize,
    pub independent_tx_count: usize,
    pub parallelization_coefficient: f64,
    pub task_group_count: usize,
    pub largest_group_size: usize,
    pub singleton_group_count: usize,
    pub critical_path_length: usize,
    pub max_achievable_parallelism: usize,
    pub parallel_speedup_factor: f64,
    pub dependency_graph_density: f64,
}

pub fn independence_coefficient(graph: &DepGraph) -> f64 {
    if graph.tx_count == 0 {
        return 1.0;
    }
    let independent = (0..graph.tx_count)
        .filter(|&tx| graph.is_independent(tx).unwrap_or(false))
        .count();
    independent as f64 / graph.tx_count as f64
}

/// Compute the full set of [`BlockMetrics`] for a dependency graph.
pub fn compute_metrics(graph: &DepGraph) -> BlockMetrics {
    let tx_count = graph.tx_count;

    // Degree-zero transactions: no incoming and no outgoing edges.
    let independent_tx_count = (0..tx_count)
        .filter(|&tx| graph.is_independent(tx).unwrap_or(false))
        .count();

    let parallelization_coefficient = if tx_count == 0 {
        1.0
    } else {
        independent_tx_count as f64 / tx_count as f64
    };

    // Weakly connected component count. `connected_components` walks the graph
    // as if every edge were undirected, which is exactly the WCC definition.
    let task_group_count = connected_components(&graph.graph);

    // Component sizes via BFS over the undirected view of the graph.
    let group_sizes = weakly_connected_component_sizes(graph);
    let largest_group_size = group_sizes.iter().copied().max().unwrap_or(0);
    let singleton_group_count = group_sizes.iter().filter(|&&size| size == 1).count();
    let (cpl, map) = compute_topo_levels(graph);

    let parallel_speedup_factor = if cpl == 0 {
        1.0
    } else {
        tx_count as f64 / cpl as f64
    };

    let dependency_graph_density = {
        let n = tx_count;
        if n < 2 {
            0.0
        } else {
            let max_edges = n * (n - 1) / 2;
            graph.edge_count() as f64 / max_edges as f64
        }
    };

    BlockMetrics {
        tx_count,
        independent_tx_count,
        parallelization_coefficient,
        task_group_count,
        largest_group_size,
        singleton_group_count,
        critical_path_length: cpl,
        max_achievable_parallelism: map,
        parallel_speedup_factor,
        dependency_graph_density,
    }
}

/// Collect the size of every weakly connected component using a breadth-first
/// search over the undirected view of the graph.
///
/// petgraph's [`connected_components`] returns only the number of components,
/// so this BFS recovers component membership. Each unvisited node seeds a new
/// component, and [`neighbors_undirected`] expands it by following edges
/// regardless of direction — i.e. the undirected view of the dependency graph.
///
/// [`neighbors_undirected`]: petgraph::graph::Graph::neighbors_undirected
fn weakly_connected_component_sizes(graph: &DepGraph) -> Vec<usize> {
    let g = &graph.graph;
    let mut visited = vec![false; g.node_count()];
    let mut sizes = Vec::new();
    let mut queue: VecDeque<_> = VecDeque::new();

    for start in g.node_indices() {
        if visited[start.index()] {
            continue;
        }

        // Seed a fresh component and BFS outward over undirected edges.
        visited[start.index()] = true;
        queue.push_back(start);
        let mut size = 0usize;

        while let Some(node) = queue.pop_front() {
            size += 1;
            for neighbor in g.neighbors_undirected(node) {
                if !visited[neighbor.index()] {
                    visited[neighbor.index()] = true;
                    queue.push_back(neighbor);
                }
            }
        }

        sizes.push(size);
    }

    sizes
}
/// Compute the longest dependency chain via single-pass topological DP.
///
/// Assigns every node a `level`:
///
/// level[root] = 1
/// level[node] = max(level[predecessors]) + 1
///
/// Returns the maximum level, i.e. the critical path length.
/// Returns `0` for an empty graph.
pub fn critical_path_length(graph: &DepGraph) -> usize {
    if graph.tx_count == 0 {
        return 0;
    }

    let topo = toposort(&graph.graph, None).expect("Dependency graph must ");

    let mut level = vec![0usize; graph.graph.node_count()];

    for node in &topo {
        let pred_max = graph
            .graph
            .neighbors_directed(*node, Direction::Incoming)
            .map(|pred| level[pred.index()])
            .max()
            .unwrap_or(0);
        level[node.index()] = pred_max + 1;
    }

    level.into_iter().max().unwrap_or(0)
}

/// Calculates peak wave width across all execution levels.
///
/// Uses [`compute_topo_levels`] to group transactions into concurrent execution waves
/// and returns the size of the largest wave.
pub fn max_achievable_parallelism(graph: &DepGraph) -> usize {
    compute_topo_levels(graph).1
}

/// Theoretical transaction-count speedup: `tx_count / critical_path_length`.
///
/// Guards: returns `1.0` when `critical_path_length == 0` (empty block).
pub fn parallel_speedup_factor(graph: &DepGraph) -> f64 {
    let cpl = compute_topo_levels(graph).0;
    if cpl == 0 {
        return 1.0;
    }
    graph.tx_count as f64 / cpl as f64
}

/// Fraction of all possible conflict edges that actually exist.
///
/// `density = edge_count / (tx_count * (tx_count - 1) / 2)`
///
/// Guards: returns `0.0` when `tx_count < 2` (no pairs possible).
pub fn dependency_graph_density(graph: &DepGraph) -> f64 {
    let n = graph.tx_count;
    if n < 2 {
        return 0.0;
    }
    let max_edges = n * (n - 1) / 2;
    graph.edge_count() as f64 / max_edges as f64
}

/// Single-pass topological DP helper returning `(critical_path_length, max_achievable_parallelism)`.
fn compute_topo_levels(graph: &DepGraph) -> (usize, usize) {
    if graph.tx_count == 0 {
        return (0, 0);
    }

    let topo = toposort(&graph.graph, None).expect("dependency graph must be acyclic");
    let mut level = vec![0usize; graph.graph.node_count()];

    for node in &topo {
        let pred_max = graph
            .graph
            .neighbors_directed(*node, Direction::Incoming)
            .map(|pred| level[pred.index()])
            .max()
            .unwrap_or(0);
        level[node.index()] = pred_max + 1;
    }

    let cpl = level.iter().copied().max().unwrap_or(0);

    let mut wave_sizes = vec![0usize; cpl + 1];
    for &l in &level {
        wave_sizes[l] += 1;
    }

    let map = wave_sizes.into_iter().max().unwrap_or(0);

    (cpl, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::types::ConflictType;

    /// Add a directed conflict edge between two transaction positions.
    fn add_edge(g: &mut DepGraph, from: usize, to: usize) {
        let a = g.node_for_tx(from).unwrap();
        let b = g.node_for_tx(to).unwrap();
        g.graph.add_edge(a, b, ConflictType::WriteAfterWrite);
    }

    #[test]
    fn empty_graph_is_fully_independent() {
        let g = DepGraph::new(0, 1);
        assert_eq!(independence_coefficient(&g), 1.0);
        let m = compute_metrics(&g);
        assert_eq!(m.tx_count, 0);
        assert_eq!(m.parallelization_coefficient, 1.0);
        assert_eq!(m.task_group_count, 0);
        assert_eq!(m.largest_group_size, 0);
        assert_eq!(m.singleton_group_count, 0);
    }

    #[test]
    fn edgeless_graph_is_fully_independent() {
        let g = DepGraph::new(4, 1);
        assert_eq!(independence_coefficient(&g), 1.0);
    }

    #[test]
    fn single_edge_marks_both_endpoints_dependent() {
        let mut g = DepGraph::new(4, 1);
        add_edge(&mut g, 0, 1);
        assert_eq!(independence_coefficient(&g), 0.5);
    }

    #[test]
    fn ten_independent_txs() {
        let g = DepGraph::new(10, 1);
        let m = compute_metrics(&g);
        assert_eq!(m.tx_count, 10);
        assert_eq!(m.independent_tx_count, 10);
        assert_eq!(m.parallelization_coefficient, 1.0);
        assert_eq!(m.task_group_count, 10);
        assert_eq!(m.largest_group_size, 1);
        assert_eq!(m.singleton_group_count, 10);
    }

    #[test]
    fn chain_of_five_plus_five_independent() {
        let mut g = DepGraph::new(10, 1);
        // Dependency chain 0 -> 1 -> 2 -> 3 -> 4; txs 5..=9 stay independent.
        for i in 0..4 {
            add_edge(&mut g, i, i + 1);
        }
        let m = compute_metrics(&g);
        assert_eq!(m.tx_count, 10);
        assert_eq!(m.independent_tx_count, 5);
        assert_eq!(m.parallelization_coefficient, 0.5);
        assert_eq!(m.task_group_count, 6);
        assert_eq!(m.largest_group_size, 5);
        assert_eq!(m.singleton_group_count, 5);
    }

    #[test]
    fn single_chain_of_ten() {
        let mut g = DepGraph::new(10, 1);
        for i in 0..9 {
            add_edge(&mut g, i, i + 1);
        }
        let m = compute_metrics(&g);
        assert_eq!(m.tx_count, 10);
        assert_eq!(m.independent_tx_count, 0);
        assert_eq!(m.parallelization_coefficient, 0.0);
        assert_eq!(m.task_group_count, 1);
        assert_eq!(m.largest_group_size, 10);
        assert_eq!(m.singleton_group_count, 0);
    }

    #[test]
    fn property_bounds_hold_over_random_graphs() {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..100 {
            let tx_count = (next() % 20 + 1) as usize; // 1..=20
            let mut g = DepGraph::new(tx_count, 1);

            let edge_attempts = next() % (tx_count as u64 * 2 + 1);
            for _ in 0..edge_attempts {
                let mut a = (next() as usize) % tx_count;
                let mut b = (next() as usize) % tx_count;
                if a != b {
                    if a > b {
                        std::mem::swap(&mut a, &mut b);
                    }
                    let node_a = g.node_for_tx(a).unwrap();
                    let node_b = g.node_for_tx(b).unwrap();
                    if !g.graph.contains_edge(node_a, node_b) {
                        add_edge(&mut g, a, b);
                    }
                }
            }

            let m = compute_metrics(&g);

            assert!((0.0..=1.0).contains(&m.parallelization_coefficient));
            assert!(m.task_group_count >= 1 && m.task_group_count <= tx_count);
            assert!(m.largest_group_size >= 1 && m.largest_group_size <= tx_count);
            assert!(m.independent_tx_count <= tx_count);
            assert!(m.critical_path_length >= 1 && m.critical_path_length <= tx_count);
            assert!((0.0..=1.0).contains(&m.dependency_graph_density));
            assert!(m.critical_path_length * m.max_achievable_parallelism >= tx_count);
            // The BFS component count must agree with `connected_components`.
            assert_eq!(
                m.task_group_count,
                weakly_connected_component_sizes(&g).len()
            );
        }
    }

    #[test]
    fn empty_block_new_metrics() {
        let g = DepGraph::new(0, 1);
        let m = compute_metrics(&g);
        assert_eq!(m.critical_path_length, 0);
        assert_eq!(m.max_achievable_parallelism, 0);
        assert_eq!(m.parallel_speedup_factor, 1.0); // guard: cpl==0
        assert_eq!(m.dependency_graph_density, 0.0); // guard: tx_count<2
    }

    #[test]
    fn fully_parallel_block_new_metrics() {
        let g = DepGraph::new(4, 1);
        let m = compute_metrics(&g);
        assert_eq!(m.critical_path_length, 1); // all at level 1
        assert_eq!(m.max_achievable_parallelism, 4); // all 4 in wave 1
        assert_eq!(m.parallel_speedup_factor, 4.0); // 4/1
        assert_eq!(m.dependency_graph_density, 0.0); // no edges
    }

    #[test]
    fn diamond_graph_new_metrics() {
        let mut g = DepGraph::new(4, 1);
        add_edge(&mut g, 0, 1);
        add_edge(&mut g, 0, 2);
        add_edge(&mut g, 1, 3);
        add_edge(&mut g, 2, 3);
        let m = compute_metrics(&g);
        // levels: 0→1, 1→2, 2→2, 3→3  ⟹  cpl=3
        assert_eq!(m.critical_path_length, 3);
        // wave widths: {1:1, 2:2, 3:1}  ⟹  peak=2
        assert_eq!(m.max_achievable_parallelism, 2);
        // 4 / 3 ≈ 1.333…
        assert!((m.parallel_speedup_factor - 4.0 / 3.0).abs() < 1e-10);
        // edges=4, max_possible=6 → 4/6 ≈ 0.6667
        assert!((m.dependency_graph_density - 4.0 / 6.0).abs() < 1e-10);
    }

    #[test]
    fn linear_chain_new_metrics() {
        // 0 -> 1 -> 2 -> 3
        let mut g = DepGraph::new(4, 1);
        for i in 0..3 {
            add_edge(&mut g, i, i + 1);
        }
        let m = compute_metrics(&g);
        assert_eq!(m.critical_path_length, 4);
        assert_eq!(m.max_achievable_parallelism, 1);
        assert_eq!(m.parallel_speedup_factor, 1.0); // 4/4 = 1.0
                                                    // edges=3, max_possible=4*3/2=6 → 3/6 = 0.5
        assert!((m.dependency_graph_density - 0.5).abs() < 1e-10);
    }
}
