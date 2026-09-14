use crate::cache::CacheError;
use crate::rpc::RpcError;
use thiserror::Error;

use super::block::BlockId;

#[derive(Error, Debug)]
pub enum FetchError {
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcError),

    #[error("cache error: {0}")]
    Cache(#[from] CacheError),

    #[error("block not found: {0:?}")]
    BlockNotFound(BlockId),

    #[error("malformed rpc response for block: missing or invalid field '{field}'")]
    MalformedResponse { field: &'static str },
}
