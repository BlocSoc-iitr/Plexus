use crate::cache::{config::CacheConfig, error::CacheError, io::read_json};
use alloy_primitives::B256;
use serde_json::Value;
use std::path::PathBuf;
use types::types::BlockContext;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderStatus {
    /// Header exists and parses into a [`BlockContext`].
    Ok,
    /// No header file on disk at the expected path.
    Missing(PathBuf),
    /// Header exists but could not be read or parsed (invalid JSON, wrong
    Malformed { path: PathBuf, reason: String },
}

/// Every field is a plain record of what was found — the audit never fails, it only reports.
#[derive(Debug, Clone)]
pub struct CacheVerificationReport {
    pub chain_id: u64,
    pub block_number: u64,
    pub header: HeaderStatus,
    pub verified_tx_files: Vec<B256>,
    pub missing_tx_files: Vec<B256>,
    pub corrupted_tx_files: Vec<(B256, PathBuf, String)>,
}

impl CacheVerificationReport {
    /// `true` only when the header is present and valid and every declared
    pub fn is_complete(&self) -> bool {
        matches!(self.header, HeaderStatus::Ok)
            && self.missing_tx_files.is_empty()
            && self.corrupted_tx_files.is_empty()
    }
}

/// Checks that a decoded trace file has the expected top-level shape produced
/// by `prestateTracer` in diff mode: both `pre` and `post` present and non-null.
fn validate_trace_shape(value: &Value) -> Result<(), String> {
    for key in ["pre", "post"] {
        match value.get(key) {
            None => return Err(format!("missing required top-level key '{key}'")),
            Some(Value::Null) => return Err(format!("top-level key '{key}' is null")),
            Some(_) => {}
        }
    }
    Ok(())
}

/// Offline audit of a block's cache directory.
/// Performs no RPC or network I/O — it only reads what is already on disk.
/// Reads `block_header.json`, then checks that every transaction it declares
/// has a present, non-empty, well-formed `tx_{hash}.json` trace file, and
/// returns a [`CacheVerificationReport`] describing the result.
pub fn verify_block_cache(
    cache: &CacheConfig,
    chain_id: u64,
    block_number: u64,
) -> CacheVerificationReport {
    let header_path = cache.block_header_path(chain_id, block_number);

    let mut verified = Vec::new();
    let mut missing = Vec::new();
    let mut corrupted = Vec::new();

    let block_ctx = match read_json::<BlockContext>(&header_path) {
        Ok(ctx) => ctx,
        Err(CacheError::NotFound(p)) => {
            return CacheVerificationReport {
                chain_id,
                block_number,
                header: HeaderStatus::Missing(p),
                verified_tx_files: verified,
                missing_tx_files: missing,
                corrupted_tx_files: corrupted,
            };
        }
        Err(CacheError::Malformed { path, source }) => {
            return CacheVerificationReport {
                chain_id,
                block_number,
                header: HeaderStatus::Malformed {
                    path,
                    reason: source.to_string(),
                },
                verified_tx_files: verified,
                missing_tx_files: missing,
                corrupted_tx_files: corrupted,
            };
        }
        Err(CacheError::Io { path, source }) => {
            // treat as Malformed/unreadable too — surface the OS error, don't panic
            return CacheVerificationReport {
                chain_id,
                block_number,
                header: HeaderStatus::Malformed {
                    path,
                    reason: source.to_string(),
                },
                verified_tx_files: verified,
                missing_tx_files: missing,
                corrupted_tx_files: corrupted,
            };
        }
    };

    for hash in &block_ctx.tx_hashes {
        let path = cache.tx_path(chain_id, block_number, hash);
        match read_json::<Value>(&path) {
            Ok(value) => match validate_trace_shape(&value) {
                Ok(()) => verified.push(*hash),
                Err(reason) => corrupted.push((*hash, path, reason)),
            },
            Err(CacheError::NotFound(_)) => missing.push(*hash),
            Err(CacheError::Malformed { path, source }) => {
                corrupted.push((*hash, path, source.to_string()))
            }
            Err(CacheError::Io { path, source }) => {
                corrupted.push((*hash, path, source.to_string()))
            }
        }
    }

    CacheVerificationReport {
        chain_id,
        block_number,
        header: HeaderStatus::Ok,
        verified_tx_files: verified,
        missing_tx_files: missing,
        corrupted_tx_files: corrupted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::io::write_json;
    use alloy_primitives::Address;
    use std::fs::{create_dir_all, write, File};
    use tempfile::{tempdir, TempDir};
    use types::types::BlockContext;

    const CHAIN_ID: u64 = 1;
    const BLOCK_NUMBER: u64 = 100;

    fn setup() -> (TempDir, CacheConfig) {
        let dir = tempdir().unwrap();
        let cache = CacheConfig::with_root(dir.path().to_path_buf());
        (dir, cache)
    }

    /// Writes a valid `block_header.json` declaring the given transaction hashes.
    fn write_header(cache: &CacheConfig, tx_hashes: Vec<B256>) {
        let ctx = BlockContext {
            number: BLOCK_NUMBER,
            hash: B256::ZERO,
            parent_hash: B256::ZERO,
            coinbase: Address::ZERO,
            chain_id: CHAIN_ID,
            timestamp: 1234567890,
            base_fee_per_gas: None,
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            tx_hashes,
            block_access_list_hash: None,
        };
        write_json(&cache.block_header_path(CHAIN_ID, BLOCK_NUMBER), &ctx).unwrap();
    }

    /// Writes a valid (non-empty JSON) trace file for a hash.
    fn write_valid_tx(cache: &CacheConfig, hash: &B256) {
        write_json(
            &cache.tx_path(CHAIN_ID, BLOCK_NUMBER, hash),
            &serde_json::json!({ "pre": {}, "post": {} }),
        )
        .unwrap();
    }

    #[test]
    fn complete_cache_reports_is_complete() {
        let (_dir, cache) = setup();
        let h1 = B256::from([0x11; 32]);
        let h2 = B256::from([0x22; 32]);
        write_header(&cache, vec![h1, h2]);
        write_valid_tx(&cache, &h1);
        write_valid_tx(&cache, &h2);

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert_eq!(report.header, HeaderStatus::Ok);
        assert_eq!(report.verified_tx_files.len(), 2);
        assert!(report.missing_tx_files.is_empty());
        assert!(report.corrupted_tx_files.is_empty());
        assert!(report.is_complete());
    }

    #[test]
    fn missing_header_reports_missing_status() {
        let (_dir, cache) = setup();

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert!(matches!(report.header, HeaderStatus::Missing(_)));
        assert!(report.verified_tx_files.is_empty());
        assert!(!report.is_complete());
    }

    #[test]
    fn malformed_header_reports_malformed_status() {
        let (_dir, cache) = setup();
        let path = cache.block_header_path(CHAIN_ID, BLOCK_NUMBER);
        create_dir_all(path.parent().unwrap()).unwrap();
        write(&path, b"this is not json").unwrap();

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert!(matches!(report.header, HeaderStatus::Malformed { .. }));
        assert!(!report.is_complete());
    }

    #[test]
    fn missing_tx_file_is_reported() {
        let (_dir, cache) = setup();
        let present = B256::from([0x11; 32]);
        let absent = B256::from([0x22; 32]);
        write_header(&cache, vec![present, absent]);
        write_valid_tx(&cache, &present);

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert_eq!(report.verified_tx_files, vec![present]);
        assert_eq!(report.missing_tx_files, vec![absent]);
        assert!(report.corrupted_tx_files.is_empty());
        assert!(!report.is_complete());
    }

    #[test]
    fn corrupted_tx_file_is_reported() {
        let (_dir, cache) = setup();
        let hash = B256::from([0x11; 32]);
        write_header(&cache, vec![hash]);
        // Invalid JSON on disk at the expected tx path.
        let tx_path = cache.tx_path(CHAIN_ID, BLOCK_NUMBER, &hash);
        create_dir_all(tx_path.parent().unwrap()).unwrap();
        write(&tx_path, b"{ broken json").unwrap();

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert!(report.verified_tx_files.is_empty());
        assert!(report.missing_tx_files.is_empty());
        assert_eq!(report.corrupted_tx_files.len(), 1);
        assert_eq!(report.corrupted_tx_files[0].0, hash);
        assert!(!report.is_complete());
    }

    #[test]
    fn empty_tx_file_is_reported_as_corrupted() {
        let (_dir, cache) = setup();
        let hash = B256::from([0x11; 32]);
        write_header(&cache, vec![hash]);
        // A zero-byte file is not valid JSON, so it must land in `corrupted`,
        // not `missing` — the file exists, it's just unparseable.
        let tx_path = cache.tx_path(CHAIN_ID, BLOCK_NUMBER, &hash);
        create_dir_all(tx_path.parent().unwrap()).unwrap();
        File::create(&tx_path).unwrap();

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert!(report.missing_tx_files.is_empty());
        assert_eq!(report.corrupted_tx_files.len(), 1);
        assert_eq!(report.corrupted_tx_files[0].0, hash);
        assert!(!report.is_complete());
    }

    #[test]
    fn tx_file_missing_post_key_is_corrupted() {
        let (_dir, cache) = setup();
        let hash = B256::from([0x11; 32]);
        write_header(&cache, vec![hash]);
        // Valid JSON, but not a diff-mode prestate trace: `post` is absent.
        write_json(
            &cache.tx_path(CHAIN_ID, BLOCK_NUMBER, &hash),
            &serde_json::json!({ "pre": {} }),
        )
        .unwrap();

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert!(report.verified_tx_files.is_empty());
        assert_eq!(report.corrupted_tx_files.len(), 1);
        assert_eq!(report.corrupted_tx_files[0].0, hash);
        assert!(report.corrupted_tx_files[0].2.contains("post"));
        assert!(!report.is_complete());
    }

    #[test]
    fn tx_file_with_null_pre_is_corrupted() {
        let (_dir, cache) = setup();
        let hash = B256::from([0x11; 32]);
        write_header(&cache, vec![hash]);
        // Both keys present, but `pre` is explicitly null.
        write_json(
            &cache.tx_path(CHAIN_ID, BLOCK_NUMBER, &hash),
            &serde_json::json!({ "pre": null, "post": {} }),
        )
        .unwrap();

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert!(report.verified_tx_files.is_empty());
        assert_eq!(report.corrupted_tx_files.len(), 1);
        assert!(report.corrupted_tx_files[0].2.contains("pre"));
        assert!(!report.is_complete());
    }

    #[test]
    fn mixed_report_counts_are_correct() {
        let (_dir, cache) = setup();
        let ok = B256::from([0x11; 32]);
        let gone = B256::from([0x22; 32]);
        let bad = B256::from([0x33; 32]);
        write_header(&cache, vec![ok, gone, bad]);

        write_valid_tx(&cache, &ok);
        // `gone` is left absent.
        let bad_path = cache.tx_path(CHAIN_ID, BLOCK_NUMBER, &bad);
        write(&bad_path, b"not json").unwrap();

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert_eq!(report.verified_tx_files, vec![ok]);
        assert_eq!(report.missing_tx_files, vec![gone]);
        assert_eq!(report.corrupted_tx_files.len(), 1);
        assert_eq!(report.corrupted_tx_files[0].0, bad);
        assert!(!report.is_complete());
    }

    #[test]
    fn empty_block_is_trivially_complete() {
        let (_dir, cache) = setup();
        write_header(&cache, vec![]);

        let report = verify_block_cache(&cache, CHAIN_ID, BLOCK_NUMBER);

        assert_eq!(report.header, HeaderStatus::Ok);
        assert!(report.verified_tx_files.is_empty());
        assert!(report.missing_tx_files.is_empty());
        assert!(report.corrupted_tx_files.is_empty());
        assert!(report.is_complete());
    }
}
