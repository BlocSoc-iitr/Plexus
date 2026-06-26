use crate::cache::CacheError;
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::io::Write;
use serde::de::DeserializeOwned;

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CacheError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CacheError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let serialized_bytes = serde_json::to_vec_pretty(value).map_err(|e| {
        CacheError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        }
    })?;
    
    let tmp_path = path.with_extension("json.tmp");

    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| CacheError::Io {
                path: tmp_path.clone(),
                source: e,
            })?;

        file.write_all(&serialized_bytes).map_err(|e| CacheError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;

        // Call .sync_all() (fsync)
        file.sync_all().map_err(|e| CacheError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
    } //closing the file so we can rename it.

    fs::rename(&tmp_path, path).map_err(|e| CacheError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    Ok(())
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, CacheError>{
    if !path.exists() {
        return Err(CacheError::NotFound(path.to_path_buf()));
    }

    let file_content = fs::read_to_string(path).map_err(|e| {
        CacheError::Io {
            path: path.to_path_buf(),
            source: e,
        }
    })?;

    let value: T = serde_json::from_str(&file_content).map_err(|e| {
        CacheError::Malformed {
            path: path.to_path_buf(),
            source: e 
        }
    })?;

    Ok(value)
}
