//PathBuf owns its data on the heap so it's easy 
//to implement cross-platform paths by modifying them.
use std::path::PathBuf;
use alloy_primitives::B256;

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