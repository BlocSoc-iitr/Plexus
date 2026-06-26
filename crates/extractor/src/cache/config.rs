#![allow(dead_code)]
//PathBuf owns its data on the heap so it's easy
//to implement cross-platform paths by modifying them.
use alloy_primitives::B256;
use std::path::PathBuf;

pub struct CacheConfig {
    pub root: PathBuf,
}

impl CacheConfig {
    //default root of dirs::home_dir()/.plexus/cache
    pub fn default() -> Self {
        let root: PathBuf = dirs::home_dir()
            .expect("cannot resolve home directory; set HOME or use CacheConfig::with_root()")
            .join(".plexus")
            .join("cache");
        Self { root }
    }
    //custom root path
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    // root/{chain_id}/{block_number}/
    pub fn block_dir(&self, chain_id: u64, block_number: u64) -> PathBuf {
        self.root
            .join(chain_id.to_string())
            .join(block_number.to_string())
    }

    // root/{chain_id}/{block_number}/block_header.json
    pub fn block_header_path(&self, chain_id: u64, block_number: u64) -> PathBuf {
        self.block_dir(chain_id, block_number)
            .join("block_header.json")
    }

    // root/{chain_id}/{block_number}/tx_{lowercase_hex_hash}.json
    pub fn tx_path(&self, chain_id: u64, block_number: u64, tx_hash: &B256) -> PathBuf {
        let filename = format!("tx_{}.json", hex::encode(tx_hash.as_slice()));
        self.block_dir(chain_id, block_number).join(filename)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn block_header_path_structure() {
        let config = CacheConfig::with_root(PathBuf::from("/tmp/plexus"));
        let path = config.block_header_path(1, 100);
        assert_eq!(path, PathBuf::from("/tmp/plexus/1/100/block_header.json"));
    }

    #[test]
    fn tx_path_filename_format() {
        let config = CacheConfig::with_root(PathBuf::from("/tmp/plexus"));
        let hash = B256::from([0xabu8; 32]);
        let path = config.tx_path(1, 100, &hash);
        let filename = path.file_name().unwrap().to_str().unwrap();
        // no 0x prefix, lowercase, wrapped in tx_...json
        assert!(filename.starts_with("tx_"));
        assert!(filename.ends_with(".json"));
        assert!(!filename.contains("0x"));
        assert_eq!(filename, filename.to_lowercase());
    }

    #[test]
    fn different_chains_and_blocks_are_isolated() {
        let config = CacheConfig::with_root(PathBuf::from("/tmp/plexus"));
        let a = config.block_dir(1, 100);
        let b = config.block_dir(137, 100); // same block, different chain
        let c = config.block_dir(1, 200); // same chain, different block
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }
}
