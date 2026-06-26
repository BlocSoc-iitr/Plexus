use crate::cache::CacheError;
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::io::Write;

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CacheError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CacheError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let serialized_bytes = serde_json::to_vec_pretty(value).map_err(|e| {
        CacheError::Malformed {
            path: path.to_path_buf(),
            source: e, 
        }
    })?;
    
    let mut tmp_path = path.to_path_buf();

    if let Some(file_name) = path.file_name(){
        let mut new_name = file_name.to_os_string();
        new_name.push(".tmp");
        tmp_path.set_file_name(new_name);

    }else {
        return Err(CacheError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid file path structure",
            ),
        });
    }
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
