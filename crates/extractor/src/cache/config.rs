//PathBuf owns its data on the heap so it's easy 
//to implement cross-platform paths by modifying them.
use std::path::PathBuf;

pub struct CacheConfig {
    pub root: PathBuf,
}