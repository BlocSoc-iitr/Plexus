#![allow(dead_code)]

use crate::cache::CacheError;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::Path;

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
    //converts a generic value into json bytes vector
    let serialized_bytes = serde_json::to_vec(value).map_err(|e| CacheError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
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

        file.write_all(&serialized_bytes)
            .map_err(|e| CacheError::Io {
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
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, CacheError> {
    if !path.exists() {
        return Err(CacheError::NotFound(path.to_path_buf()));
    }

    //Read the entire file to a String in memory.
    let file_content = fs::read_to_string(path).map_err(|e| CacheError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    //deserializing the String to the requested type T.
    let value: T = serde_json::from_str(&file_content).map_err(|e| CacheError::Malformed {
        path: path.to_path_buf(),
        source: e,
    })?;

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Dummy {
        value: u32,
        label: String,
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.json");
        let original = Dummy {
            value: 42,
            label: "plexus".into(),
        };

        write_json(&path, &original).unwrap();
        let result: Dummy = read_json(&path).unwrap();

        assert_eq!(result, original);
    }

    #[test]
    fn no_tmp_file_after_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.json");
        let value = Dummy {
            value: 1,
            label: "atomic".into(),
        };

        write_json(&path, &value).unwrap();

        // tmp file must not exist after successful rename
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        assert!(!tmp.exists());
    }

    #[test]
    fn read_missing_file_returns_not_found() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ghost.json");

        let err = read_json::<Dummy>(&path).unwrap_err();

        assert!(matches!(err, CacheError::NotFound(_)));
    }

    #[test]
    fn read_malformed_json_returns_malformed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"this is not json").unwrap();

        let err = read_json::<Dummy>(&path).unwrap_err();

        assert!(matches!(err, CacheError::Malformed { .. }));
    }
}
