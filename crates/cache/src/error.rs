use std::path::PathBuf;



// Custom error types 
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache file not found: {0}")]
    NotFound(PathBuf),

    #[error("malformed JSON at {path}: {source}")]
    MalformedJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("storage IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialize(serde_json::Error),
}