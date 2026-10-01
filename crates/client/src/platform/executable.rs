//! The file side of replacing the client's executable, shared by the
//! platform implementations (and the tests' fake platform).
//!
//! Beside `client.exe` an update leaves `client.old.exe`, the previous
//! version, until the new one has completed a handshake; a rollback leaves
//! `client.failed.exe`, the version that did not. Renaming works while the
//! executable is running (deleting it does not), which is why the running
//! file is always moved aside rather than overwritten.

use std::io;
use std::path::{Path, PathBuf};

/// `client.exe` → `client.old.exe`: the previous version during an update.
pub fn old_path(exe: &Path) -> PathBuf {
    with_suffix(exe, "old")
}

/// `client.exe` → `client.failed.exe`: the new version after a rollback.
pub fn failed_path(exe: &Path) -> PathBuf {
    with_suffix(exe, "failed")
}

fn with_suffix(exe: &Path, suffix: &str) -> PathBuf {
    let stem = exe.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let name = match exe.extension() {
        Some(extension) => format!("{stem}.{suffix}.{}", extension.to_string_lossy()),
        None => format!("{stem}.{suffix}"),
    };
    exe.with_file_name(name)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Moves `exe` aside to `old_path(exe)` and `new` into its place. On
/// failure `exe` is put back.
pub fn swap(exe: &Path, new: &Path) -> io::Result<()> {
    let old = old_path(exe);
    // Left by an earlier update that never completed.
    remove_if_present(&old)?;
    std::fs::rename(exe, &old)?;
    if let Err(error) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(error);
    }
    Ok(())
}

/// Undoes [`swap`]: moves `exe` aside to `failed_path(exe)` and the
/// previous version back into its place. On failure `exe` is put back.
pub fn restore(exe: &Path) -> io::Result<()> {
    let old = old_path(exe);
    if !old.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("there is no previous version at {}", old.display()),
        ));
    }
    let failed = failed_path(exe);
    remove_if_present(&failed)?;
    std::fs::rename(exe, &failed)?;
    if let Err(error) = std::fs::rename(&old, exe) {
        let _ = std::fs::rename(&failed, exe);
        return Err(error);
    }
    Ok(())
}

/// Deletes what an update or a rollback left beside `exe`. Only call it
/// from a process that is not running either of them.
pub fn remove_leftovers(exe: &Path) -> io::Result<()> {
    remove_if_present(&old_path(exe))?;
    remove_if_present(&failed_path(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn leftovers_are_named_after_the_executable() {
        let exe = Path::new(r"C:\Fake Install\client.exe");
        assert_eq!(old_path(exe), Path::new(r"C:\Fake Install\client.old.exe"));
        assert_eq!(failed_path(exe), Path::new(r"C:\Fake Install\client.failed.exe"));
        assert_eq!(old_path(Path::new("/opt/sc/client")), Path::new("/opt/sc/client.old"));
    }

    #[test]
    fn swap_then_restore_then_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("client.exe");
        let new = dir.path().join("client.update.tmp");
        std::fs::write(&exe, "old version").unwrap();
        std::fs::write(&new, "new version").unwrap();
        // Left over from an update that never completed.
        std::fs::write(old_path(&exe), "stale").unwrap();

        swap(&exe, &new).unwrap();
        assert_eq!(read(&exe), "new version");
        assert_eq!(read(&old_path(&exe)), "old version");
        assert!(!new.exists());

        restore(&exe).unwrap();
        assert_eq!(read(&exe), "old version");
        assert_eq!(read(&failed_path(&exe)), "new version");
        assert!(!old_path(&exe).exists());

        remove_leftovers(&exe).unwrap();
        assert!(!failed_path(&exe).exists());
        assert_eq!(read(&exe), "old version");
        // Nothing left to remove is fine.
        remove_leftovers(&exe).unwrap();
    }

    #[test]
    fn a_failed_swap_leaves_the_executable_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("client.exe");
        std::fs::write(&exe, "old version").unwrap();
        let missing = dir.path().join("no-such-download.tmp");
        assert!(swap(&exe, &missing).is_err());
        assert_eq!(read(&exe), "old version");
        assert!(!old_path(&exe).exists());
    }

    #[test]
    fn restoring_without_a_previous_version_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("client.exe");
        std::fs::write(&exe, "new version").unwrap();
        assert_eq!(restore(&exe).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(read(&exe), "new version");
        assert!(!failed_path(&exe).exists());
    }
}
