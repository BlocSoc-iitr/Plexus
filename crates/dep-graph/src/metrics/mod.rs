//! Metrics derived from dependency graphs, block metadata, complete block-level
//! BAL access, and transaction-attributed state writes.

use crate::graph::DepGraph;
use alloy_primitives::Address;
use petgraph::algo::connected_components;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use types::types::{AccessSet, BlockAccess, BlockContext, StateKey};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockMetrics {
    pub tx_count: usize,
    pub independent_tx_count: usize,
    pub parallelization_coefficient: f64,
    pub task_group_count: usize,
    pub largest_group_size: usize,
    pub singleton_group_count: usize,
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

pub fn gas_utilization_ratio(ctx: &BlockContext) -> f64 {
    if ctx.gas_limit == 0 {
        return 0.0;
    }

    ctx.gas_used as f64 / ctx.gas_limit as f64
}

pub fn eth_burned_wei(ctx: &BlockContext) -> u128 {
    ctx.base_fee_per_gas.unwrap_or(0) * ctx.gas_used as u128
}

pub fn tx_count(ctx: &BlockContext) -> usize {
    ctx.tx_hashes.len()
}

/// Number of distinct accounts represented anywhere in the block BAL.
///
/// `BlockAccess::touched` currently contains touched-only accounts, so reads
/// and writes must also contribute their addresses.
pub fn unique_accounts_touched(block: &BlockAccess) -> usize {
    let mut accounts: HashSet<Address> = block.touched.iter().copied().collect();

    accounts.extend(block.reads().iter().map(StateKey::address));
    accounts.extend(block.writes.iter().map(|write| write.key.address()));

    accounts.len()
}

/// Number of distinct storage slots read or written anywhere in the block.
///
/// Block-level reads and all write-log positions, including system writes, are
/// included. Balance, nonce, and code keys are excluded.
pub fn unique_storage_slots_touched(block: &BlockAccess) -> usize {
    let mut slots = HashSet::new();

    for key in block.reads() {
        if let StateKey::StorageSlot { address, slot } = key {
            slots.insert((*address, *slot));
        }
    }

    for write in &block.writes {
        if let StateKey::StorageSlot { address, slot } = &write.key {
            slots.insert((*address, *slot));
        }
    }

    slots.len()
}

fn storage_slot_order(left: &StateKey, right: &StateKey) -> Ordering {
    match (left, right) {
        (
            StateKey::StorageSlot {
                address: left_address,
                slot: left_slot,
            },
            StateKey::StorageSlot {
                address: right_address,
                slot: right_slot,
            },
        ) => left_address
            .as_slice()
            .cmp(right_address.as_slice())
            .then_with(|| left_slot.as_slice().cmp(right_slot.as_slice())),
        _ => Ordering::Equal,
    }
}

/// Storage slots ranked by the number of distinct transactions that wrote them.
pub fn hot_slots(
    access_sets: &[AccessSet],
    ctx: &BlockContext,
    top_k: usize,
) -> Vec<(StateKey, usize)> {
    if top_k == 0 || access_sets.is_empty() {
        return Vec::new();
    }

    let mut writer_counts: HashMap<StateKey, usize> = HashMap::new();

    for access_set in access_sets {
        for key in &access_set.writes {
            if matches!(key, StateKey::StorageSlot { .. }) && key.address() != ctx.coinbase {
                *writer_counts.entry(key.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut ranked: Vec<(StateKey, usize)> = writer_counts.into_iter().collect();

    ranked.sort_by(|(left_key, left_count), (right_key, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| storage_slot_order(left_key, right_key))
    });

    ranked.truncate(top_k);
    ranked
}

/// Gini coefficient over transaction-attributed write counts per active address.
pub fn write_concentration_gini(access_sets: &[AccessSet], ctx: &BlockContext) -> f64 {
    let mut writes_per_address: HashMap<Address, usize> = HashMap::new();

    for access_set in access_sets {
        for key in &access_set.writes {
            let address = key.address();

            if address != ctx.coinbase {
                *writes_per_address.entry(address).or_insert(0) += 1;
            }
        }
    }

    if writes_per_address.len() < 2 {
        return 0.0;
    }

    let mut counts: Vec<f64> = writes_per_address
        .into_values()
        .map(|count| count as f64)
        .collect();

    counts.sort_by(f64::total_cmp);

    let population_size = counts.len() as f64;
    let total_writes: f64 = counts.iter().sum();

    if total_writes == 0.0 {
        return 0.0;
    }

    let weighted_sum: f64 = counts
        .iter()
        .enumerate()
        .map(|(index, count)| (index as f64 + 1.0) * count)
        .sum();

    let gini = (2.0 * weighted_sum) / (population_size * total_writes)
        - (population_size + 1.0) / population_size;

    gini.clamp(0.0, 1.0)
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

    BlockMetrics {
        tx_count,
        independent_tx_count,
        parallelization_coefficient,
        task_group_count,
        largest_group_size,
        singleton_group_count,
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, B256, U256};
    use types::types::{ConflictType, ReadAttribution, TxPosition, WriteEntry, WriteValue};

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn b256(byte: u8) -> B256 {
        B256::from([byte; 32])
    }

    fn storage_key(address_byte: u8, slot_byte: u8) -> StateKey {
        StateKey::StorageSlot {
            address: addr(address_byte),
            slot: b256(slot_byte),
        }
    }

    fn context(coinbase: Address) -> BlockContext {
        BlockContext {
            number: 1,
            hash: b256(0xAA),
            parent_hash: b256(0xBB),
            coinbase,
            chain_id: 1,
            timestamp: 1,
            base_fee_per_gas: Some(1),
            gas_limit: 30_000_000,
            gas_used: 1,
            tx_hashes: Vec::new(),
            block_access_list_hash: None,
        }
    }

    fn access_set(tx_index: usize, writes: Vec<StateKey>) -> AccessSet {
        AccessSet {
            tx_index,
            tx_hash: b256(tx_index as u8),
            reads: ReadAttribution::PerTransaction(HashSet::new()),
            writes: writes.into_iter().collect(),
        }
    }

    fn write_entry(key: StateKey) -> WriteEntry {
        let value = match &key {
            StateKey::StorageSlot { .. } => WriteValue::Storage(B256::ZERO),
            StateKey::Balance(_) => WriteValue::Balance(U256::ZERO),
            StateKey::Nonce(_) => WriteValue::Nonce(0),
            StateKey::Code(_) => WriteValue::Code(Bytes::new()),
        };

        WriteEntry {
            position: TxPosition::Transaction(0),
            key,
            value,
        }
    }

    fn block_access(
        reads: Vec<StateKey>,
        writes: Vec<StateKey>,
        touched: Vec<Address>,
    ) -> BlockAccess {
        BlockAccess::new(
            context(addr(0xFE)),
            writes.into_iter().map(write_entry).collect(),
            reads.into_iter().collect(),
            touched.into_iter().collect(),
        )
    }

    fn approximately_equal(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

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
                let a = (next() as usize) % tx_count;
                let b = (next() as usize) % tx_count;
                if a != b {
                    add_edge(&mut g, a, b);
                }
            }

            let m = compute_metrics(&g);

            assert!((0.0..=1.0).contains(&m.parallelization_coefficient));
            assert!(m.task_group_count >= 1 && m.task_group_count <= tx_count);
            assert!(m.largest_group_size >= 1 && m.largest_group_size <= tx_count);
            assert!(m.independent_tx_count <= tx_count);
            // The BFS component count must agree with `connected_components`.
            assert_eq!(
                m.task_group_count,
                weakly_connected_component_sizes(&g).len()
            );
        }
    }

    #[test]
    fn empty_bal_metrics_return_zero() {
        let block = block_access(Vec::new(), Vec::new(), Vec::new());
        let ctx = context(addr(0xFE));

        assert_eq!(unique_accounts_touched(&block), 0);
        assert_eq!(unique_storage_slots_touched(&block), 0);
        assert!(hot_slots(&[], &ctx, 10).is_empty());
        assert_eq!(write_concentration_gini(&[], &ctx), 0.0);
    }

    #[test]
    fn unique_accounts_include_touched_reads_and_writes() {
        let block = block_access(
            vec![storage_key(0x02, 0x01)],
            vec![StateKey::Balance(addr(0x03)), StateKey::Nonce(addr(0x03))],
            vec![addr(0x01), addr(0x01)],
        );

        assert_eq!(unique_accounts_touched(&block), 3);
    }

    #[test]
    fn duplicate_account_accesses_count_once() {
        let block = block_access(
            vec![storage_key(0x01, 0x01), storage_key(0x01, 0x02)],
            vec![
                StateKey::Balance(addr(0x01)),
                StateKey::Nonce(addr(0x01)),
                StateKey::Code(addr(0x01)),
            ],
            vec![addr(0x01)],
        );

        assert_eq!(unique_accounts_touched(&block), 1);
    }

    #[test]
    fn unique_storage_slots_include_reads_and_writes() {
        let block = block_access(
            vec![storage_key(0x01, 0x01)],
            vec![storage_key(0x02, 0x02)],
            Vec::new(),
        );

        assert_eq!(unique_storage_slots_touched(&block), 2);
    }

    #[test]
    fn repeated_storage_writes_count_once() {
        let repeated = storage_key(0x01, 0x01);

        let block = block_access(Vec::new(), vec![repeated.clone(), repeated], Vec::new());

        assert_eq!(unique_storage_slots_touched(&block), 1);
    }

    #[test]
    fn non_storage_keys_are_excluded_from_storage_slot_count() {
        let block = block_access(
            vec![storage_key(0x01, 0x01), StateKey::Balance(addr(0x02))],
            vec![
                StateKey::Balance(addr(0x03)),
                StateKey::Nonce(addr(0x04)),
                StateKey::Code(addr(0x05)),
            ],
            Vec::new(),
        );

        assert_eq!(unique_storage_slots_touched(&block), 1);
    }

    #[test]
    fn hot_slots_rank_by_transaction_writer_count() {
        let shared = storage_key(0x01, 0x01);
        let single = storage_key(0x02, 0x01);
        let coinbase_slot = storage_key(0xFE, 0x01);

        let access_sets = vec![
            access_set(
                0,
                vec![
                    shared.clone(),
                    shared.clone(),
                    single.clone(),
                    coinbase_slot.clone(),
                ],
            ),
            access_set(1, vec![shared.clone(), coinbase_slot]),
        ];

        let result = hot_slots(&access_sets, &context(addr(0xFE)), 10);

        assert_eq!(result, vec![(shared, 2), (single, 1)]);
    }

    #[test]
    fn hot_slots_break_ties_deterministically() {
        let lower_slot = storage_key(0x01, 0x02);
        let higher_slot = storage_key(0x01, 0x09);
        let higher_address = storage_key(0x02, 0x01);

        let access_sets = vec![access_set(
            0,
            vec![higher_address, higher_slot.clone(), lower_slot.clone()],
        )];

        let result = hot_slots(&access_sets, &context(addr(0xFE)), 2);

        assert_eq!(result, vec![(lower_slot, 1), (higher_slot, 1)]);
    }
    #[test]
    fn gini_is_zero_for_one_or_equally_written_addresses() {
        let ctx = context(addr(0xFE));

        let one_address = vec![access_set(
            0,
            vec![StateKey::Balance(addr(0x01)), StateKey::Nonce(addr(0x01))],
        )];
        assert_eq!(write_concentration_gini(&one_address, &ctx), 0.0);

        let equal = vec![access_set(
            0,
            vec![StateKey::Balance(addr(0x01)), StateKey::Balance(addr(0x02))],
        )];
        assert_eq!(write_concentration_gini(&equal, &ctx), 0.0);
    }

    #[test]
    fn gini_detects_unequal_writes_and_excludes_coinbase() {
        let ctx = context(addr(0xFE));

        let access_sets = vec![
            access_set(
                0,
                vec![
                    StateKey::Balance(addr(0x01)),
                    StateKey::Nonce(addr(0x01)),
                    StateKey::Balance(addr(0x02)),
                    StateKey::Balance(addr(0xFE)),
                ],
            ),
            access_set(1, vec![StateKey::Code(addr(0x01))]),
        ];

        let result = write_concentration_gini(&access_sets, &ctx);

        assert!(approximately_equal(result, 0.25));
        assert!((0.0..=1.0).contains(&result));
    }

    #[test]
    fn gas_utilization_ratio_handles_zero_half_and_full_blocks() {
        let mut ctx = context(addr(0xFE));
        ctx.gas_limit = 100;

        ctx.gas_used = 0;
        assert_eq!(gas_utilization_ratio(&ctx), 0.0);

        ctx.gas_used = 50;
        assert_eq!(gas_utilization_ratio(&ctx), 0.5);

        ctx.gas_used = 100;
        assert_eq!(gas_utilization_ratio(&ctx), 1.0);
    }

    #[test]
    fn gas_utilization_ratio_returns_zero_for_zero_gas_limit() {
        let mut ctx = context(addr(0xFE));
        ctx.gas_limit = 0;
        ctx.gas_used = 100;

        assert_eq!(gas_utilization_ratio(&ctx), 0.0);
    }

    #[test]
    fn eth_burned_is_zero_without_base_fee() {
        let mut ctx = context(addr(0xFE));
        ctx.base_fee_per_gas = None;
        ctx.gas_used = 1_000_000;

        assert_eq!(eth_burned_wei(&ctx), 0);
    }

    #[test]
    fn eth_burned_multiplies_base_fee_by_gas_used() {
        let mut ctx = context(addr(0xFE));
        ctx.base_fee_per_gas = Some(10);
        ctx.gas_used = 21_000;

        assert_eq!(eth_burned_wei(&ctx), 210_000);
    }

    #[test]
    fn tx_count_returns_number_of_transaction_hashes() {
        let mut ctx = context(addr(0xFE));

        assert_eq!(tx_count(&ctx), 0);

        ctx.tx_hashes = vec![b256(0x01), b256(0x02), b256(0x03)];

        assert_eq!(tx_count(&ctx), 3);
    }

    #[test]
    fn hot_slots_returns_empty_when_top_k_is_zero() {
        let access_sets = vec![access_set(0, vec![storage_key(0x01, 0x01)])];
        let ctx = context(addr(0xFE));

        assert!(hot_slots(&access_sets, &ctx, 0).is_empty());
    }
}
