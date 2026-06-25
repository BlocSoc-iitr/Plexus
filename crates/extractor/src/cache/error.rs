use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("Cache item not found at {0}")]
    NotFound(PathBuf),

    #[error("Malformed cache file at {path}: {source}")]
    Malformed { 
        path: PathBuf, 
        source: serde_json::Error 
    },

    #[error("IO error at {path}: {source}")]
    Io { 
        path: PathBuf, 
        source: std::io::Error 
    }
}