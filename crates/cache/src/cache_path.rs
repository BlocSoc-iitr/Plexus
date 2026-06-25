use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn block_header_path_is_correct() {
        let root = PathBuf::from("/home/user/.plexus/cache");
        let path = block_header_path(&root, 1, 19_000_000);
        assert_eq!(
            path,
            PathBuf::from("/home/user/.plexus/cache/1/19000000/block_header.json")
        );
    }

    #[test]
    fn tx_path_is_correct() {
        let root = PathBuf::from("/home/user/.plexus/cache");
        let path = tx_path(&root, 1, 19_000_000, "0xabc123");
        assert_eq!(
            path,
            PathBuf::from("/home/user/.plexus/cache/1/19000000/tx_0xabc123.json")
        );
    }
}