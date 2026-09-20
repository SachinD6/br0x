//! Tiny JSON file persistence with atomic writes.
//! Shared by the session store and preferences.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, Write};
use std::path::Path;

pub fn save<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let bytes =
        serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Per process tmp name: two writers must not share one scratch file.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    sync_parent(path);
    Ok(())
}

/// fsync the directory entry so the rename itself survives a crash.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

pub fn load<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
