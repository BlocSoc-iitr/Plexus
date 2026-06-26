use crate::cache::CacheError;
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::io::Write;
use serde::de::DeserializeOwned;

/// Writes cache on disk using an atomic-write pattern.
///
/// To prevent file corruption from sudden crashes or power failures, this function
/// writes the data to a temporary file (`.json.tmp`), flushes it to physical disk via `fsync`,
/// and then atomically renames it to the final target path.
///
/// # Errors
///
/// Returns a [`CacheError::Io`] if:
/// * The parent directory cannot be created.
/// * Serialization fails (treated as an IO/storage error).
/// * The temporary file cannot be opened, written to, or synchronized.
/// * The final rename operation fails.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CacheError> {
    //checks for parent directory and creates it if not present.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CacheError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    //converts a generic value into a pretty-printed jason bytes vector 
    let serialized_bytes = serde_json::to_vec_pretty(value).map_err(|e| {
        CacheError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        }
    })?;

    //creates temporary file path.(eg-"data.json.tmp")
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

        //force the OS to flush object in memory to disk.
        file.sync_all().map_err(|e| CacheError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
    } //scope of the file dropped so we can rename it.

    //Perform an atomic swap, replacing the temp file to json. 
    fs::rename(&tmp_path, path).map_err(|e| CacheError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    Ok(())
}

/// Reads and deserializes a data structure from a JSON file.
///
/// # Errors
///
/// Returns:
/// * [`CacheError::NotFound`] if the target file does not exist.
/// * [`CacheError::Io`] if the file cannot be read from disk due to OS or permission errors.
/// * [`CacheError::Malformed`] if the file contains invalid JSON syntax or mismatched data types.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, CacheError>{
    if !path.exists() {
        return Err(CacheError::NotFound(path.to_path_buf()));
    }

    //Read the entire file to a String in memory.
    let file_content = fs::read_to_string(path).map_err(|e| {
        CacheError::Io {
            path: path.to_path_buf(),
            source: e,
        }
    })?;

    //deserializing the String to the requested type T.
    let value: T = serde_json::from_str(&file_content).map_err(|e| {
        CacheError::Malformed {
            path: path.to_path_buf(),
            source: e 
        }
    })?;

    Ok(value)
}
