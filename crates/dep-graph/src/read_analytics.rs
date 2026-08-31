use std::collections::{HashMap, HashSet};

use types::types::{BlockAccess, StateKey, TxPosition, WriteEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caveats {
    pub block_granularity: &'static str,
    pub contamination: &'static str,
}

impl Default for Caveats {
    fn default() -> Self {
        Self {
            block_granularity:
                "BAL reads are block-level; per-transaction attribution is unavailable",
            contamination:
                "Counts may include no-op / reverted-inner-call reads and are upper bounds",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotReadCount {
    pub address: alloy_primitives::Address,
    pub slot: alloy_primitives::B256,
    pub read_count: usize,
}

#[derive(Debug, Clone)]
pub struct HotReadSlots {
    pub slots: Vec<SlotReadCount>,
    pub caveats: Caveats,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContractReadWrite {
    pub address: alloy_primitives::Address,
    pub read_count: usize,
    pub write_count: usize,
    pub read_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct ReadWriteRatio {
    pub contracts: Vec<ContractReadWrite>,
    pub caveats: Caveats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheCandidate {
    pub address: alloy_primitives::Address,
    pub slot: alloy_primitives::B256,
    pub read_count: usize,
    pub write_count: usize,
}

#[derive(Debug, Clone)]
pub struct CacheCandidacy {
    pub candidates: Vec<CacheCandidate>,
    pub caveats: Caveats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentionHotspot {
    pub address: alloy_primitives::Address,
    pub slot: alloy_primitives::B256,
    pub read_count: usize,
    pub write_count: usize,
}

#[derive(Debug, Clone)]
pub struct ContentionHotspots {
    pub hotspots: Vec<ContentionHotspot>,
    pub caveats: Caveats,
}

#[derive(Debug, Clone)]
pub struct PrefetchWorkingSet {
    pub keys: HashSet<StateKey>,
    pub caveats: Caveats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MevSandwichSignal {
    pub address: alloy_primitives::Address,
    pub slot: alloy_primitives::B256,
    pub may_be_noop_write: bool,
}

#[derive(Debug, Clone)]
pub struct MevSandwichSignals {
    pub signals: Vec<MevSandwichSignal>,
    pub caveats: Caveats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedUnchanged {
    pub address: alloy_primitives::Address,
    pub wasted_access_list_entry: bool,
}

#[derive(Debug, Clone)]
pub struct TouchedUnchangedAccounts {
    pub accounts: Vec<TouchedUnchanged>,
    pub caveats: Caveats,
}

#[derive(Debug, Clone)]
pub struct ReadAnalytics {
    pub hot_read_slots: HotReadSlots,
    pub read_write_ratio: ReadWriteRatio,
    pub cache_candidacy: CacheCandidacy,
    pub contention_hotspots: ContentionHotspots,
    pub prefetch_working_set: PrefetchWorkingSet,
    pub mev_sandwich_signals: MevSandwichSignals,
    pub touched_unchanged: TouchedUnchangedAccounts,
}

pub fn compute_read_analytics(block: &BlockAccess) -> ReadAnalytics {
    ReadAnalytics {
        hot_read_slots: compute_hot_read_slots(block),
        read_write_ratio: compute_read_write_ratio(block),
        cache_candidacy: compute_cache_candidacy(block),
        contention_hotspots: compute_contention_hotspots(block),
        prefetch_working_set: compute_prefetch_working_set(block),
        mev_sandwich_signals: compute_mev_sandwich_signals(block),
        touched_unchanged: compute_touched_unchanged(block),
    }
}

fn key_address(key: &StateKey) -> alloy_primitives::Address {
    match key {
        StateKey::StorageSlot { address, .. }
        | StateKey::Balance(address)
        | StateKey::Nonce(address)
        | StateKey::Code(address) => *address,
    }
}

fn storage_address_slot(key: &StateKey) -> Option<(alloy_primitives::Address, alloy_primitives::B256)> {
    match key {
        StateKey::StorageSlot { address, slot } => Some((*address, *slot)),
        _ => None,
    }
}

fn read_counts(block: &BlockAccess) -> HashMap<(alloy_primitives::Address, alloy_primitives::B256), usize> {
    let mut counts: HashMap<(alloy_primitives::Address, alloy_primitives::B256), usize> = HashMap::new();
    for key in block.reads() {
        if let Some((addr, slot)) = storage_address_slot(key) {
            *counts.entry((addr, slot)).or_insert(0) += 1;
        }
    }
    counts
}

fn write_counts(block: &BlockAccess) -> HashMap<(alloy_primitives::Address, alloy_primitives::B256), usize> {
    let mut counts: HashMap<(alloy_primitives::Address, alloy_primitives::B256), usize> = HashMap::new();
    for entry in &block.writes {
        if let Some((addr, slot)) = storage_address_slot(&entry.key) {
            *counts.entry((addr, slot)).or_insert(0) += 1;
        }
    }
    counts
}

fn per_address_counts(
    block: &BlockAccess,
) -> HashMap<alloy_primitives::Address, (usize, usize)> {
    let mut map: HashMap<alloy_primitives::Address, (usize, usize)> = HashMap::new();
    for key in block.reads() {
        let entry = map.entry(key_address(key)).or_insert((0, 0));
        entry.0 += 1;
    }
    for entry in &block.writes {
        let e = map.entry(key_address(&entry.key)).or_insert((0, 0));
        e.1 += 1;
    }
    map
}

fn write_addresses(block: &BlockAccess) -> HashSet<alloy_primitives::Address> {
    block.writes.iter().map(|w| key_address(&w.key)).collect()
}

fn read_addresses(block: &BlockAccess) -> HashSet<alloy_primitives::Address> {
    block.reads().iter().map(|k| key_address(k)).collect()
}

fn tx_writes_with_position(
    block: &BlockAccess,
) -> Vec<(&WriteEntry, usize)> {
    block
        .writes
        .iter()
        .filter_map(|w| match w.position {
            TxPosition::Transaction(i) => Some((w, i)),
            _ => None,
        })
        .collect()
}

fn compute_hot_read_slots(block: &BlockAccess) -> HotReadSlots {
    let counts = read_counts(block);
    let mut slots: Vec<SlotReadCount> = counts
        .into_iter()
        .map(|((address, slot), read_count)| SlotReadCount {
            address,
            slot,
            read_count,
        })
        .collect();
    slots.sort_by(|a, b| b.read_count.cmp(&a.read_count));

    HotReadSlots {
        slots,
        caveats: Caveats::default(),
    }
}

fn compute_read_write_ratio(block: &BlockAccess) -> ReadWriteRatio {
    let counts = per_address_counts(block);
    let mut contracts: Vec<ContractReadWrite> = counts
        .into_iter()
        .map(|(address, (read_count, write_count))| {
            let total = read_count + write_count;
            let read_ratio = if total == 0 {
                0.0
            } else {
                read_count as f64 / total as f64
            };
            ContractReadWrite {
                address,
                read_count,
                write_count,
                read_ratio,
            }
        })
        .collect();
    contracts.sort_by(|a, b| b.read_count.cmp(&a.read_count));

    ReadWriteRatio {
        contracts,
        caveats: Caveats::default(),
    }
}

fn compute_cache_candidacy(block: &BlockAccess) -> CacheCandidacy {
    let rcounts = read_counts(block);
    let wcounts = write_counts(block);

    let mut all_slots: HashSet<(alloy_primitives::Address, alloy_primitives::B256)> =
        HashSet::new();
    all_slots.extend(rcounts.keys());
    all_slots.extend(wcounts.keys());

    let mut candidates: Vec<CacheCandidate> = all_slots
        .into_iter()
        .filter_map(|(address, slot)| {
            let rc = *rcounts.get(&(address, slot)).unwrap_or(&0);
            let wc = *wcounts.get(&(address, slot)).unwrap_or(&0);
            if rc > 0 && wc == 0 {
                Some(CacheCandidate {
                    address,
                    slot,
                    read_count: rc,
                    write_count: wc,
                })
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by(|a, b| b.read_count.cmp(&a.read_count));

    CacheCandidacy {
        candidates,
        caveats: Caveats::default(),
    }
}

fn compute_contention_hotspots(block: &BlockAccess) -> ContentionHotspots {
    let rcounts = read_counts(block);
    let wcounts = write_counts(block);

    let mut hotspots: Vec<ContentionHotspot> = rcounts
        .keys()
        .filter_map(|&(address, slot)| {
            let rc = *rcounts.get(&(address, slot)).unwrap_or(&0);
            let wc = *wcounts.get(&(address, slot)).unwrap_or(&0);
            if wc > 0 {
                Some(ContentionHotspot {
                    address,
                    slot,
                    read_count: rc,
                    write_count: wc,
                })
            } else {
                None
            }
        })
        .collect();
    hotspots.sort_by(|a, b| {
        (b.read_count + b.write_count).cmp(&(a.read_count + a.write_count))
    });

    ContentionHotspots {
        hotspots,
        caveats: Caveats::default(),
    }
}

fn compute_prefetch_working_set(block: &BlockAccess) -> PrefetchWorkingSet {
    PrefetchWorkingSet {
        keys: block.reads().clone(),
        caveats: Caveats::default(),
    }
}

fn compute_mev_sandwich_signals(block: &BlockAccess) -> MevSandwichSignals {
    let read_keys: HashSet<StateKey> = block.reads().clone();
    let tx_writes: Vec<(&WriteEntry, usize)> = tx_writes_with_position(block);

    let mut signals: Vec<MevSandwichSignal> = tx_writes
        .iter()
        .filter_map(|(w, _tx_idx)| {
            if read_keys.contains(&w.key) {
                if let Some((address, slot)) = storage_address_slot(&w.key) {
                    let write_count_to_slot = tx_writes
                        .iter()
                        .filter(|(tw, _)| {
                            storage_address_slot(&tw.key)
                                == Some((address, slot))
                        })
                        .count();
                    let may_be_noop_write = write_count_to_slot <= 1;
                    return Some(MevSandwichSignal {
                        address,
                        slot,
                        may_be_noop_write,
                    });
                }
            }
            None
        })
        .collect();
    signals.sort_by(|a, b| a.address.cmp(&b.address).then(a.slot.cmp(&b.slot)));
    signals.dedup_by(|a, b| a.address == b.address && a.slot == b.slot);

    MevSandwichSignals {
        signals,
        caveats: Caveats::default(),
    }
}

fn compute_touched_unchanged(block: &BlockAccess) -> TouchedUnchangedAccounts {
    let raddrs = read_addresses(block);
    let waddrs = write_addresses(block);

    let mut accounts: Vec<TouchedUnchanged> = block
        .touched
        .iter()
        .filter_map(|&addr| {
            if !raddrs.contains(&addr) && !waddrs.contains(&addr) {
                Some(TouchedUnchanged {
                    address: addr,
                    wasted_access_list_entry: true,
                })
            } else {
                None
            }
        })
        .collect();
    accounts.sort_by(|a, b| a.address.cmp(&b.address));

    TouchedUnchangedAccounts {
        accounts,
        caveats: Caveats::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use types::types::{BlockContext, WriteValue};

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn slot(byte: u8) -> B256 {
        B256::from([byte; 32])
    }

    fn storage_key(a: u8, s: u8) -> StateKey {
        StateKey::StorageSlot {
            address: addr(a),
            slot: slot(s),
        }
    }

    fn balance_key(a: u8) -> StateKey {
        StateKey::Balance(addr(a))
    }

    fn ctx(tx_count: usize) -> BlockContext {
        BlockContext {
            number: 21_000_000,
            hash: slot(0xB1),
            parent_hash: slot(0xB0),
            coinbase: addr(0xFE),
            chain_id: 1,
            timestamp: 1_700_000_000,
            base_fee_per_gas: Some(7),
            gas_limit: 30_000_000,
            gas_used: 12_345,
            tx_hashes: (0..tx_count).map(|i| slot(i as u8)).collect(),
            block_access_list_hash: None,
        }
    }

    fn fixture_amm_block() -> BlockAccess {
        let mut reads = HashSet::new();
        reads.insert(storage_key(0xA0, 0x01));
        reads.insert(storage_key(0xA0, 0x02));
        reads.insert(storage_key(0xA0, 0x03));
        reads.insert(storage_key(0xB0, 0x01));
        reads.insert(storage_key(0xB0, 0x02));
        reads.insert(storage_key(0xC0, 0x01));
        reads.insert(balance_key(0xD1));

        let writes = vec![
            WriteEntry {
                position: TxPosition::Transaction(0),
                key: storage_key(0xA0, 0x01),
                value: WriteValue::Storage(slot(0xAA)),
            },
            WriteEntry {
                position: TxPosition::Transaction(1),
                key: storage_key(0xC0, 0x10),
                value: WriteValue::Storage(slot(0xBB)),
            },
            WriteEntry {
                position: TxPosition::Transaction(2),
                key: storage_key(0xA0, 0x04),
                value: WriteValue::Storage(slot(0xCC)),
            },
        ];

        let mut touched = HashSet::new();
        touched.insert(addr(0xD1));
        touched.insert(addr(0xD2));
        touched.insert(addr(0xD3));

        BlockAccess::new(ctx(3), writes, reads, touched)
    }

    fn fixture_write_heavy() -> BlockAccess {
        let mut reads = HashSet::new();
        reads.insert(storage_key(0xE0, 0x01));
        reads.insert(storage_key(0xF0, 0x01));
        reads.insert(storage_key(0xF0, 0x02));

        let writes = vec![
            WriteEntry {
                position: TxPosition::Transaction(0),
                key: storage_key(0xF0, 0x01),
                value: WriteValue::Storage(slot(0xAA)),
            },
            WriteEntry {
                position: TxPosition::Transaction(1),
                key: storage_key(0xF0, 0x02),
                value: WriteValue::Storage(slot(0xBB)),
            },
            WriteEntry {
                position: TxPosition::Transaction(2),
                key: storage_key(0xF0, 0x03),
                value: WriteValue::Storage(slot(0xCC)),
            },
            WriteEntry {
                position: TxPosition::Transaction(3),
                key: storage_key(0xE0, 0x01),
                value: WriteValue::Storage(slot(0xDD)),
            },
        ];

        BlockAccess::new(ctx(4), writes, reads, HashSet::new())
    }

    fn fixture_read_only() -> BlockAccess {
        let mut reads = HashSet::new();
        reads.insert(storage_key(0x10, 0x01));
        reads.insert(storage_key(0x10, 0x02));
        reads.insert(storage_key(0x20, 0x01));

        BlockAccess::new(ctx(0), Vec::new(), reads, HashSet::new())
    }

    fn fixture_system_writers() -> BlockAccess {
        let mut reads = HashSet::new();
        reads.insert(storage_key(0xA0, 0x01));

        let writes = vec![
            WriteEntry {
                position: TxPosition::PreTransaction,
                key: storage_key(0xA0, 0x01),
                value: WriteValue::Storage(slot(0xAA)),
            },
            WriteEntry {
                position: TxPosition::PostTransaction,
                key: storage_key(0xA0, 0x02),
                value: WriteValue::Storage(slot(0xBB)),
            },
        ];

        BlockAccess::new(ctx(0), writes, reads, HashSet::new())
    }

    fn fixture_sandwich_pattern() -> BlockAccess {
        let mut reads = HashSet::new();
        reads.insert(storage_key(0xA1, 0x01));
        reads.insert(storage_key(0xB1, 0x01));

        let writes = vec![
            WriteEntry {
                position: TxPosition::Transaction(0),
                key: storage_key(0xA1, 0x01),
                value: WriteValue::Storage(slot(0xAA)),
            },
            WriteEntry {
                position: TxPosition::Transaction(1),
                key: storage_key(0xA1, 0x01),
                value: WriteValue::Storage(slot(0xBB)),
            },
            WriteEntry {
                position: TxPosition::Transaction(2),
                key: storage_key(0xB1, 0x01),
                value: WriteValue::Storage(slot(0xCC)),
            },
        ];

        BlockAccess::new(ctx(3), writes, reads, HashSet::new())
    }

    #[test]
    fn hot_read_slots_sorted_descending() {
        let block = fixture_amm_block();
        let hot = compute_hot_read_slots(&block);

        assert_eq!(hot.slots.len(), 6);
        for s in &hot.slots {
            assert_eq!(s.read_count, 1);
        }

        let addrs: Vec<_> = hot.slots.iter().map(|s| s.address).collect();
        assert!(addrs.contains(&addr(0xA0)));
        assert!(addrs.contains(&addr(0xB0)));
        assert!(addrs.contains(&addr(0xC0)));

        assert_eq!(hot.caveats.block_granularity, Caveats::default().block_granularity);
    }

    #[test]
    fn read_write_ratio_per_contract() {
        let block = fixture_amm_block();
        let rw = compute_read_write_ratio(&block);
        let map: HashMap<_, _> = rw.contracts.iter().map(|c| (c.address, c)).collect();

        let a0 = map.get(&addr(0xA0)).unwrap();
        assert_eq!(a0.read_count, 3);
        assert_eq!(a0.write_count, 2);
        assert!((a0.read_ratio - 0.6).abs() < f64::EPSILON);

        let b0 = map.get(&addr(0xB0)).unwrap();
        assert_eq!(b0.read_count, 2);
        assert_eq!(b0.write_count, 0);
        assert!((b0.read_ratio - 1.0).abs() < f64::EPSILON);

        let c0 = map.get(&addr(0xC0)).unwrap();
        assert_eq!(c0.read_count, 1);
        assert_eq!(c0.write_count, 1);
        assert!((c0.read_ratio - 0.5).abs() < f64::EPSILON);

        let d1 = map.get(&addr(0xD1)).unwrap();
        assert_eq!(d1.read_count, 1);
        assert_eq!(d1.write_count, 0);
        assert!((d1.read_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cache_candidacy_only_read_only_slots() {
        let block = fixture_amm_block();
        let cc = compute_cache_candidacy(&block);

        assert_eq!(cc.candidates.len(), 5);
        let candidate_set: HashSet<_> = cc
            .candidates
            .iter()
            .map(|c| (c.address, c.slot))
            .collect();
        assert!(candidate_set.contains(&(addr(0xA0), slot(0x02))));
        assert!(candidate_set.contains(&(addr(0xA0), slot(0x03))));
        assert!(candidate_set.contains(&(addr(0xB0), slot(0x01))));
        assert!(candidate_set.contains(&(addr(0xB0), slot(0x02))));
        assert!(candidate_set.contains(&(addr(0xC0), slot(0x01))));
        assert!(!candidate_set.contains(&(addr(0xA0), slot(0x01))));
    }

    #[test]
    fn contention_hotspots_read_and_written() {
        let block = fixture_amm_block();
        let ch = compute_contention_hotspots(&block);

        assert_eq!(ch.hotspots.len(), 1);
        assert_eq!(ch.hotspots[0].address, addr(0xA0));
        assert_eq!(ch.hotspots[0].slot, slot(0x01));
        assert_eq!(ch.hotspots[0].read_count, 1);
        assert_eq!(ch.hotspots[0].write_count, 1);
    }

    #[test]
    fn prefetch_working_set_contains_all_read_keys() {
        let block = fixture_amm_block();
        let pws = compute_prefetch_working_set(&block);

        assert_eq!(pws.keys.len(), 7);
        assert!(pws.keys.contains(&storage_key(0xA0, 0x01)));
        assert!(pws.keys.contains(&storage_key(0xA0, 0x02)));
        assert!(pws.keys.contains(&storage_key(0xA0, 0x03)));
        assert!(pws.keys.contains(&storage_key(0xB0, 0x01)));
        assert!(pws.keys.contains(&storage_key(0xB0, 0x02)));
        assert!(pws.keys.contains(&storage_key(0xC0, 0x01)));
        assert!(pws.keys.contains(&balance_key(0xD1)));
    }

    #[test]
    fn mev_sandwich_signal_detects_read_then_written() {
        let block = fixture_amm_block();
        let mev = compute_mev_sandwich_signals(&block);

        assert_eq!(mev.signals.len(), 1);
        assert_eq!(mev.signals[0].address, addr(0xA0));
        assert_eq!(mev.signals[0].slot, slot(0x01));
        assert!(mev.signals[0].may_be_noop_write);
    }

    #[test]
    fn touched_unchanged_only_unreferenced_accounts() {
        let block = fixture_amm_block();
        let tu = compute_touched_unchanged(&block);

        assert_eq!(tu.accounts.len(), 2);
        let addrs: Vec<_> = tu.accounts.iter().map(|a| a.address).collect();
        assert!(addrs.contains(&addr(0xD2)));
        assert!(addrs.contains(&addr(0xD3)));
        assert!(!addrs.contains(&addr(0xD1)));

        for a in &tu.accounts {
            assert!(a.wasted_access_list_entry);
        }
    }

    #[test]
    fn empty_block_produces_empty_analytics() {
        let block = BlockAccess::new(ctx(0), Vec::new(), HashSet::new(), HashSet::new());
        let analytics = compute_read_analytics(&block);

        assert!(analytics.hot_read_slots.slots.is_empty());
        assert!(analytics.read_write_ratio.contracts.is_empty());
        assert!(analytics.cache_candidacy.candidates.is_empty());
        assert!(analytics.contention_hotspots.hotspots.is_empty());
        assert!(analytics.prefetch_working_set.keys.is_empty());
        assert!(analytics.mev_sandwich_signals.signals.is_empty());
        assert!(analytics.touched_unchanged.accounts.is_empty());
    }

    #[test]
    fn write_heavy_all_reads_are_contention_hotspots() {
        let block = fixture_write_heavy();
        let ch = compute_contention_hotspots(&block);

        assert_eq!(ch.hotspots.len(), 3);
        let set: HashSet<_> = ch
            .hotspots
            .iter()
            .map(|h| (h.address, h.slot))
            .collect();
        assert!(set.contains(&(addr(0xE0), slot(0x01))));
        assert!(set.contains(&(addr(0xF0), slot(0x01))));
        assert!(set.contains(&(addr(0xF0), slot(0x02))));
    }

    #[test]
    fn write_heavy_no_cache_candidates() {
        let block = fixture_write_heavy();
        let cc = compute_cache_candidacy(&block);
        assert!(cc.candidates.is_empty());
    }

    #[test]
    fn write_heavy_read_write_ratios() {
        let block = fixture_write_heavy();
        let rw = compute_read_write_ratio(&block);
        let map: HashMap<_, _> = rw.contracts.iter().map(|c| (c.address, c)).collect();

        let e0 = map.get(&addr(0xE0)).unwrap();
        assert_eq!(e0.read_count, 1);
        assert_eq!(e0.write_count, 1);
        assert!((e0.read_ratio - 0.5).abs() < f64::EPSILON);

        let f0 = map.get(&addr(0xF0)).unwrap();
        assert_eq!(f0.read_count, 2);
        assert_eq!(f0.write_count, 3);
        assert!((f0.read_ratio - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn write_heavy_mev_signals_all_three() {
        let block = fixture_write_heavy();
        let mev = compute_mev_sandwich_signals(&block);

        assert_eq!(mev.signals.len(), 3);
        for s in &mev.signals {
            assert!(s.may_be_noop_write);
        }
    }

    #[test]
    fn write_heavy_no_touched_unchanged() {
        let block = fixture_write_heavy();
        let tu = compute_touched_unchanged(&block);
        assert!(tu.accounts.is_empty());
    }

    #[test]
    fn fixture_b_hot_read_slots_correct_count() {
        let block = fixture_write_heavy();
        let hot = compute_hot_read_slots(&block);
        assert_eq!(hot.slots.len(), 3);
    }

    #[test]
    fn read_only_all_reads_are_cache_candidates() {
        let block = fixture_read_only();
        let cc = compute_cache_candidacy(&block);
        assert_eq!(cc.candidates.len(), 3);
    }

    #[test]
    fn read_only_no_contention() {
        let block = fixture_read_only();
        let ch = compute_contention_hotspots(&block);
        assert!(ch.hotspots.is_empty());
    }

    #[test]
    fn read_only_no_mev_signals() {
        let block = fixture_read_only();
        let mev = compute_mev_sandwich_signals(&block);
        assert!(mev.signals.is_empty());
    }

    #[test]
    fn system_writers_detected_in_write_counts() {
        let block = fixture_system_writers();
        let wc = write_counts(&block);
        assert_eq!(wc.len(), 2);
        assert_eq!(wc.get(&(addr(0xA0), slot(0x01))), Some(&1));
        assert_eq!(wc.get(&(addr(0xA0), slot(0x02))), Some(&1));
    }

    #[test]
    fn system_writers_no_mev_signals() {
        let block = fixture_system_writers();
        let mev = compute_mev_sandwich_signals(&block);
        assert!(mev.signals.is_empty());
    }

    #[test]
    fn system_writers_still_contention_if_read() {
        let block = fixture_system_writers();
        let ch = compute_contention_hotspots(&block);
        assert_eq!(ch.hotspots.len(), 1);
        assert_eq!(ch.hotspots[0].address, addr(0xA0));
        assert_eq!(ch.hotspots[0].slot, slot(0x01));
    }

    #[test]
    fn sandwich_pattern_two_writes_not_noop() {
        let block = fixture_sandwich_pattern();
        let mev = compute_mev_sandwich_signals(&block);

        let map: HashMap<_, _> = mev
            .signals
            .iter()
            .map(|s| ((s.address, s.slot), s))
            .collect();

        let a = map.get(&(addr(0xA1), slot(0x01))).unwrap();
        assert!(!a.may_be_noop_write);

        let b = map.get(&(addr(0xB1), slot(0x01))).unwrap();
        assert!(b.may_be_noop_write);
    }

    #[test]
    fn all_metrics_carry_caveats() {
        let block = fixture_amm_block();
        let analytics = compute_read_analytics(&block);

        assert!(!analytics.hot_read_slots.caveats.block_granularity.is_empty());
        assert!(!analytics.hot_read_slots.caveats.contamination.is_empty());
        assert!(!analytics.read_write_ratio.caveats.block_granularity.is_empty());
        assert!(!analytics.cache_candidacy.caveats.block_granularity.is_empty());
        assert!(!analytics.contention_hotspots.caveats.block_granularity.is_empty());
        assert!(!analytics.prefetch_working_set.caveats.block_granularity.is_empty());
        assert!(!analytics.mev_sandwich_signals.caveats.block_granularity.is_empty());
        assert!(!analytics.touched_unchanged.caveats.block_granularity.is_empty());
    }

    #[test]
    fn no_metric_implies_per_tx_attribution() {
        let block = fixture_amm_block();
        assert!(!block.reads.is_exact());
        assert!(block.exact_reads().is_none());

        let analytics = compute_read_analytics(&block);
        assert!(analytics
            .hot_read_slots
            .caveats
            .block_granularity
            .contains("per-transaction"));
    }

    #[test]
    fn duplicate_inserts_into_read_set_deduplicate() {
        let mut reads = HashSet::new();
        reads.insert(storage_key(0xA0, 0x01));
        reads.insert(storage_key(0xA0, 0x01));
        reads.insert(storage_key(0xA0, 0x01));

        let block = BlockAccess::new(ctx(0), Vec::new(), reads, HashSet::new());
        let hot = compute_hot_read_slots(&block);

        assert_eq!(hot.slots.len(), 1);
        assert_eq!(hot.slots[0].read_count, 1);
    }
}
