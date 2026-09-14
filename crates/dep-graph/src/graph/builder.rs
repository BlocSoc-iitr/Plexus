use std::collections::HashSet;

use alloy_primitives::Address;
use types::types::{AccessSet, BlockContext, ConflictType, StateKey};

use super::{errors::DepGraphError, DepGraph};
/// # Errors
///
/// * [`DepGraphError::TxCountMismatch`] — `access_sets.len()` disagrees with
///   `ctx.tx_hashes.len()`.
/// * [`DepGraphError::TxIndexMismatch`] — `access_sets[i].tx_index != i`;
///   inputs must be sorted by transaction index with no gaps, because edge
///   direction is derived from slice position.
pub fn build_graph(
    access_sets: &[AccessSet],
    ctx: &BlockContext,
) -> Result<DepGraph, DepGraphError> {
    validate_inputs(access_sets, ctx)?;

    let n = access_sets.len();
    let mut dep_graph = DepGraph::new(n, ctx.number);
    let coinbase = &ctx.coinbase;

    for (i, tx_i) in access_sets.iter().enumerate() {
        if tx_i.is_empty() {
            continue;
        }

        for (j, tx_j) in access_sets.iter().enumerate().skip(i + 1) {
            let Some(conflict) = detect_conflict(tx_i, tx_j, coinbase) else {
                continue;
            };

            let node_i = dep_graph.node_for_tx(i)?;
            let node_j = dep_graph.node_for_tx(j)?;
            dep_graph.graph.add_edge(node_i, node_j, conflict);
        }
    }

    Ok(dep_graph)
}

fn detect_conflict(tx_i: &AccessSet, tx_j: &AccessSet, coinbase: &Address) -> Option<ConflictType> {
    if intersects_excluding_coinbase(&tx_i.writes, &tx_j.writes, coinbase) {
        return Some(ConflictType::WriteAfterWrite);
    }

    if intersects_excluding_coinbase(&tx_i.writes, tx_j.reads.keys(), coinbase) {
        return Some(ConflictType::ReadAfterWrite);
    }

    if let Some(reads_i) = tx_i.exact_reads() {
        if intersects_excluding_coinbase(reads_i, &tx_j.writes, coinbase) {
            return Some(ConflictType::WriteAfterRead);
        }
    }

    None
}

fn intersects_excluding_coinbase(
    a: &HashSet<StateKey>,
    b: &HashSet<StateKey>,
    coinbase: &Address,
) -> bool {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    small
        .intersection(large)
        .any(|key| !touches_coinbase(key, coinbase))
}

fn touches_coinbase(key: &StateKey, coinbase: &Address) -> bool {
    let address = match key {
        StateKey::StorageSlot { address, .. } => address,
        StateKey::Balance(address) | StateKey::Nonce(address) | StateKey::Code(address) => address,
    };
    address == coinbase
}

fn validate_inputs(access_sets: &[AccessSet], ctx: &BlockContext) -> Result<(), DepGraphError> {
    if access_sets.len() != ctx.tx_hashes.len() {
        return Err(DepGraphError::TxCountMismatch {
            access_set_count: access_sets.len(),
            block_tx_count: ctx.tx_hashes.len(),
            block_number: ctx.number,
        });
    }
    for (position, access) in access_sets.iter().enumerate() {
        if access.tx_index != position {
            return Err(DepGraphError::TxIndexMismatch {
                position,
                tx_index: access.tx_index,
            });
        }
    }
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::independence_coefficient;
    use alloy_primitives::B256;
    use petgraph::visit::EdgeRef;
    use std::sync::Arc;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn b256(byte: u8) -> B256 {
        B256::from([byte; 32])
    }

    fn slot_key(addr_byte: u8, slot_byte: u8) -> StateKey {
        StateKey::StorageSlot {
            address: addr(addr_byte),
            slot: b256(slot_byte),
        }
    }

    fn balance_key(addr_byte: u8) -> StateKey {
        StateKey::Balance(addr(addr_byte))
    }

    fn keys(items: Vec<StateKey>) -> HashSet<StateKey> {
        items.into_iter().collect()
    }

    fn access(tx_index: usize, reads: Vec<StateKey>, writes: Vec<StateKey>) -> AccessSet {
        AccessSet {
            tx_index,
            tx_hash: b256(tx_index as u8),
            reads: ReadAttributionKind::exact(reads),
            writes: keys(writes),
        }
    }

    fn access_bal(tx_index: usize, reads: Vec<StateKey>, writes: Vec<StateKey>) -> AccessSet {
        AccessSet {
            tx_index,
            tx_hash: b256(tx_index as u8),
            reads: ReadAttributionKind::block(reads),
            writes: keys(writes),
        }
    }

    struct ReadAttributionKind;
    impl ReadAttributionKind {
        fn exact(items: Vec<StateKey>) -> types::types::ReadAttribution {
            types::types::ReadAttribution::PerTransaction(keys(items))
        }
        fn block(items: Vec<StateKey>) -> types::types::ReadAttribution {
            types::types::ReadAttribution::BlockLevel(Arc::new(keys(items)))
        }
    }

    const COINBASE_BYTE: u8 = 0xFE;

    fn ctx(tx_count: usize) -> BlockContext {
        BlockContext {
            number: 1,
            hash: b256(0xAA),
            parent_hash: b256(0xAB),
            coinbase: addr(COINBASE_BYTE),
            chain_id: 1,
            timestamp: 1_700_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            gas_limit: 30_000_000,
            gas_used: 21_000,
            tx_hashes: (0..tx_count).map(|i| b256(i as u8)).collect(),
            block_access_list_hash: None,
        }
    }

    fn edge_set(g: &DepGraph) -> HashSet<(usize, usize, ConflictType)> {
        g.graph
            .edge_references()
            .map(|e| (g.graph[e.source()], g.graph[e.target()], *e.weight()))
            .collect()
    }

    #[test]
    fn five_tx_prototype_scenario_exact_edge_set() {
        let a_s1 = || slot_key(0xA0, 1);
        let b_s2 = || slot_key(0xB0, 2);

        let sets = vec![
            access(0, vec![], vec![a_s1()]),
            access(1, vec![], vec![a_s1()]),
            access(2, vec![a_s1()], vec![b_s2()]),
            access(3, vec![b_s2()], vec![]),
            access(4, vec![], vec![b_s2()]),
        ];

        let g = build_graph(&sets, &ctx(5)).unwrap();

        let expected: HashSet<_> = [
            (0, 1, ConflictType::WriteAfterWrite),
            (0, 2, ConflictType::ReadAfterWrite),
            (1, 2, ConflictType::ReadAfterWrite),
            (2, 3, ConflictType::ReadAfterWrite),
            (2, 4, ConflictType::WriteAfterWrite),
            (3, 4, ConflictType::WriteAfterRead),
        ]
        .into_iter()
        .collect();

        assert_eq!(edge_set(&g), expected);
        assert_eq!(g.edge_count(), 6);
    }

    // ── Coinbase exclusion ───────────────────────────────────────────────

    #[test]
    fn coinbase_balance_in_every_writes_produces_zero_edges() {
        let coinbase_bal = || balance_key(COINBASE_BYTE);
        let sets = vec![
            access(0, vec![], vec![coinbase_bal()]),
            access(1, vec![], vec![coinbase_bal()]),
            access(2, vec![coinbase_bal()], vec![coinbase_bal()]),
        ];

        let g = build_graph(&sets, &ctx(3)).unwrap();
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn coinbase_storage_and_nonce_keys_are_also_excluded() {
        let sets = vec![
            access(
                0,
                vec![],
                vec![
                    slot_key(COINBASE_BYTE, 1),
                    StateKey::Nonce(addr(COINBASE_BYTE)),
                ],
            ),
            access(
                1,
                vec![slot_key(COINBASE_BYTE, 1)],
                vec![slot_key(COINBASE_BYTE, 1)],
            ),
        ];

        let g = build_graph(&sets, &ctx(2)).unwrap();
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn real_conflict_survives_alongside_coinbase_noise() {
        let sets = vec![
            access(
                0,
                vec![],
                vec![balance_key(COINBASE_BYTE), slot_key(0xA0, 1)],
            ),
            access(
                1,
                vec![],
                vec![balance_key(COINBASE_BYTE), slot_key(0xA0, 1)],
            ),
        ];

        let g = build_graph(&sets, &ctx(2)).unwrap();
        assert_eq!(
            edge_set(&g),
            [(0, 1, ConflictType::WriteAfterWrite)]
                .into_iter()
                .collect()
        );
    }

    // ── Read attribution: BAL vs exact ───────────────────────────────────

    #[test]
    fn block_level_reads_produce_no_war_edges_but_exact_reads_do() {
        let key = || slot_key(0xA0, 1);

        // BAL mode: tx0 "reads" key (block-level), tx1 writes it → no WAR.
        let bal_sets = vec![
            access_bal(0, vec![key()], vec![]),
            access_bal(1, vec![], vec![key()]),
        ];
        let g_bal = build_graph(&bal_sets, &ctx(2)).unwrap();
        assert_eq!(g_bal.edge_count(), 0);

        // Exact mode: identical topology → exactly one WAR edge 0 → 1.
        let exact_sets = vec![
            access(0, vec![key()], vec![]),
            access(1, vec![], vec![key()]),
        ];
        let g_exact = build_graph(&exact_sets, &ctx(2)).unwrap();
        assert_eq!(
            edge_set(&g_exact),
            [(0, 1, ConflictType::WriteAfterRead)].into_iter().collect()
        );
    }

    #[test]
    fn block_level_reads_still_produce_raw_edges() {
        let key = || slot_key(0xA0, 1);
        let sets = vec![
            access_bal(0, vec![], vec![key()]),
            access_bal(1, vec![key()], vec![]),
        ];

        let g = build_graph(&sets, &ctx(2)).unwrap();
        assert_eq!(
            edge_set(&g),
            [(0, 1, ConflictType::ReadAfterWrite)].into_iter().collect()
        );
    }

    // ── Deduplication ────────────────────────────────────────────────────

    #[test]
    fn multiple_conflicting_slots_produce_single_edge() {
        let sets = vec![
            access(
                0,
                vec![],
                vec![slot_key(0xA0, 1), slot_key(0xA0, 2), balance_key(0xC0)],
            ),
            access(
                1,
                vec![],
                vec![slot_key(0xA0, 1), slot_key(0xA0, 2), balance_key(0xC0)],
            ),
        ];

        let g = build_graph(&sets, &ctx(2)).unwrap();
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn waw_takes_precedence_over_raw_and_war() {
        let sets = vec![
            access(
                0,
                vec![slot_key(0xA0, 3)],
                vec![slot_key(0xA0, 1), slot_key(0xA0, 2)],
            ),
            access(
                1,
                vec![slot_key(0xA0, 2)],
                vec![slot_key(0xA0, 1), slot_key(0xA0, 3)],
            ),
        ];

        let g = build_graph(&sets, &ctx(2)).unwrap();
        assert_eq!(
            edge_set(&g),
            [(0, 1, ConflictType::WriteAfterWrite)]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn independence_coefficient_is_one_for_fully_non_overlapping_sets() {
        let sets = vec![
            access(0, vec![slot_key(0xA0, 1)], vec![slot_key(0xA0, 2)]),
            access(1, vec![slot_key(0xB0, 1)], vec![slot_key(0xB0, 2)]),
            access(2, vec![slot_key(0xC0, 1)], vec![slot_key(0xC0, 2)]),
            access(3, vec![balance_key(0xD0)], vec![balance_key(0xD1)]),
        ];

        let g = build_graph(&sets, &ctx(4)).unwrap();
        assert_eq!(g.edge_count(), 0);
        assert_eq!(independence_coefficient(&g), 1.0);
    }

    #[test]
    fn independence_coefficient_reflects_partial_conflicts() {
        // tx0 ↔ tx1 conflict; tx2, tx3 independent → 2/4 = 0.5.
        let sets = vec![
            access(0, vec![], vec![slot_key(0xA0, 1)]),
            access(1, vec![], vec![slot_key(0xA0, 1)]),
            access(2, vec![], vec![slot_key(0xB0, 1)]),
            access(3, vec![], vec![slot_key(0xC0, 1)]),
        ];

        let g = build_graph(&sets, &ctx(4)).unwrap();
        assert_eq!(independence_coefficient(&g), 0.5);
    }

    #[test]
    fn empty_block_builds_empty_graph() {
        let g = build_graph(&[], &ctx(0)).unwrap();
        assert_eq!(g.tx_count, 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn tx_count_mismatch_is_rejected() {
        let sets = vec![access(0, vec![], vec![])];
        let err = build_graph(&sets, &ctx(3)).unwrap_err();
        assert_eq!(
            err,
            DepGraphError::TxCountMismatch {
                access_set_count: 1,
                block_tx_count: 3,
                block_number: 1,
            }
        );
    }

    #[test]
    fn out_of_order_tx_index_is_rejected() {
        let sets = vec![access(1, vec![], vec![]), access(0, vec![], vec![])];
        let err = build_graph(&sets, &ctx(2)).unwrap_err();
        assert_eq!(
            err,
            DepGraphError::TxIndexMismatch {
                position: 0,
                tx_index: 1,
            }
        );
    }

    #[test]
    fn edge_direction_always_points_from_lower_to_higher_index() {
        let sets = vec![
            access(0, vec![slot_key(0xA0, 1)], vec![]),
            access(1, vec![], vec![slot_key(0xA0, 1)]),
        ];
        let g = build_graph(&sets, &ctx(2)).unwrap();
        for edge in g.graph.edge_references() {
            assert!(g.graph[edge.source()] < g.graph[edge.target()]);
        }
    }
}
