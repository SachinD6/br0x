//! Tiny JSON file persistence with atomic writes.
//! Shared by the session store, bookmarks and preferences.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn save<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let bytes =
        serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = scratch_path(path);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    // The rename replaces the target, so the scratch carries its mode.
    inherit_mode(path, &tmp);
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Do not leave a copy of the payload behind.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    sync_parent(path);
    Ok(())
}

/// Per process scratch file next to the target, so the rename stays inside
/// one filesystem. The whole file name is part of the scratch name, so
/// `a.json` and `a.txt` never share one.
fn scratch_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}.tmp.{}", std::process::id()))
}

/// Keep the target's mode across the replace. A file the user made private
/// must not come back readable to everyone.
#[cfg(unix)]
fn inherit_mode(target: &Path, tmp: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(target) {
        let mode = meta.permissions().mode();
        let _ = std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn inherit_mode(_target: &Path, _tmp: &Path) {}

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

/// Move a corrupt file aside so the next save starts clean and the old bytes
/// stay recoverable. Returns where it went.
pub fn quarantine_corrupt(path: &Path) -> io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let target = path.with_file_name(format!("{name}.corrupt.{stamp}"));
    std::fs::rename(path, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("br0x-test-json-file").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scratch_path_differs_per_file_and_process() {
        let json = scratch_path(Path::new("/d/prefs.json"));
        let db = scratch_path(Path::new("/d/prefs.db"));
        assert_ne!(json, db);
        assert_eq!(json.parent(), Some(Path::new("/d")));
        assert!(json.to_string_lossy().contains(&std::process::id().to_string()));
    }

    #[test]
    fn failed_rename_leaves_no_scratch_file() {
        let dir = dir("rename-fail");
        let target = dir.join("data.json");
        // A directory cannot be replaced by a file, so the rename fails.
        std::fs::create_dir_all(&target).unwrap();
        assert!(save(&target, &"x").is_err());
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["data.json".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_replaces_previous_content() {
        let dir = dir("replace");
        let path = dir.join("data.json");
        save(&path, &"first").unwrap();
        save(&path, &"second").unwrap();
        assert_eq!(load::<String>(&path).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_target_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir("mode");
        let path = dir.join("data.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        save(&path, &"value").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_keeps_the_original_bytes() {
        let dir = dir("quarantine");
        let path = dir.join("data.json");
        std::fs::write(&path, b"{not json").unwrap();
        let moved = quarantine_corrupt(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read(&moved).unwrap(), b"{not json");
        assert!(moved.file_name().unwrap().to_string_lossy().starts_with("data.json.corrupt."));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
