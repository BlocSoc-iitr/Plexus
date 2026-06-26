use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
pub mod error;
use error::CacheError;

use serde::{de::DeserializeOwned, Serialize};
/// Returns the path for a block header JSON file.
pub fn block_header_path(root: &Path, chain_id: u64, block_number: u64) -> PathBuf {
    root.join(chain_id.to_string())
        .join(block_number.to_string())
        .join("block_header.json")
}

/// Returns the path for a transaction JSON file.
pub fn tx_path(root: &Path, chain_id: u64, block_number: u64, tx_hash: &str) -> PathBuf {
    root.join(chain_id.to_string())
        .join(block_number.to_string())
        .join(format!("tx_{}.json", tx_hash))
}


// CacheConfig struct
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub root: PathBuf,
}

impl Default for CacheConfig {
    fn default() -> Self {
        let home = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: home.join(".plexus").join("cache"),
        }
    }
}

impl CacheConfig {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn block_header_path(&self, chain_id: u64, block_number: u64) -> PathBuf {
        block_header_path(&self.root, chain_id, block_number)
    }

    pub fn tx_path(&self, chain_id: u64, block_number: u64, tx_hash: &str) -> PathBuf {
        tx_path(&self.root, chain_id, block_number, tx_hash)
    }
}




// Generic read_json<T> / write_json<T>
// Atomic write via .tmp + fsync + rename

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, CacheError> {
    if !path.exists() {
        return Err(CacheError::NotFound(path.to_owned()));
    }

    let content = fs::read_to_string(path)
        .map_err(CacheError::Io)?;

    serde_json::from_str(&content).map_err(|e| CacheError::MalformedJson {
        path: path.to_owned(),
        source: e,
    })
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CacheError> {
    // Ensure parent directories exist
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(CacheError::Io)?;
    }

    // Write to a .tmp file in the SAME directory 
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(value)
        .map_err(CacheError::Serialize)?;

    let mut file = fs::File::create(&tmp_path).map_err(CacheError::Io)?;
    file.write_all(json.as_bytes()).map_err(CacheError::Io)?;

    // fsync: flush OS write buffers to physical disk before rename
    file.sync_all().map_err(CacheError::Io)?;

    // Atomic rename: either old file or new file exists, never a partial state
    fs::rename(&tmp_path, path).map_err(CacheError::Io)?;

    Ok(())
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct BlockHeader {
        number: u64,
        hash: String,
    }

    #[test]
    fn block_header_path_correct() {
        let root = PathBuf::from("/cache");
        assert_eq!(
            block_header_path(&root, 1, 19_000_000),
            PathBuf::from("/cache/1/19000000/block_header.json")
        );
    }

    #[test]
    fn tx_path_correct() {
        let root = PathBuf::from("/cache");
        assert_eq!(
            tx_path(&root, 1, 19_000_000, "0xabc"),
            PathBuf::from("/cache/1/19000000/tx_0xabc.json")
        );
    }

    #[test]
    fn cache_config_custom_root() {
        let dir = tempdir().unwrap();
        let cfg = CacheConfig::new(dir.path().to_path_buf());
        let p = cfg.block_header_path(1, 100);
        assert_eq!(p, dir.path().join("1/100/block_header.json"));
    }
    #[test]
    fn roundtrip_block_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1/100/block_header.json");
        let header = BlockHeader {
            number: 100,
            hash: "0xdeadbeef".into(),
        };

        write_json(&path, &header).unwrap();
        let read_back: BlockHeader = read_json(&path).unwrap();
        assert_eq!(header, read_back);
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        // Deeply nested path — dirs don't exist yet
        let path = dir.path().join("1/19000000/block_header.json");
        let header = BlockHeader { number: 1, hash: "0x1".into() };
        write_json(&path, &header).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn tmp_file_is_cleaned_up_after_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("block_header.json");
        let header = BlockHeader { number: 1, hash: "0x1".into() };
        write_json(&path, &header).unwrap();
        // .tmp file must not remain after successful write
        assert!(!path.with_extension("tmp").exists());
    }


    #[test]
    fn missing_file_gives_not_found() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ghost.json");
        let result: Result<BlockHeader, _> = read_json(&path);
        assert!(matches!(result, Err(CacheError::NotFound(_))));
    }

    #[test]
    fn bad_json_gives_malformed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, b"not json at all!!!").unwrap();
        let result: Result<BlockHeader, _> = read_json(&path);
        assert!(matches!(result, Err(CacheError::MalformedJson { .. })));
    }
}