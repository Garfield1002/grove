//! Atomic file replacement.
//!
//! Both files Grove writes — app-owned `state.toml` and the user-owned
//! `config.toml` it edits surgically — are replaced the same way: serialize
//! into a temp file *in the same directory*, fsync it, then `rename(2)` over
//! the target. A crash mid-write leaves the previous file intact and a reader
//! never sees a half-written document (ARCHITECTURE.md §4).

use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// Replace `path` with `text`, atomically. Creates the parent directory.
pub fn write(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::io(format!("could not create {}", dir.display()), e))?;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "grove.tmp".to_string());
    let temp = dir.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        temp_counter()
    ));

    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::io(format!("could not write {}", temp.display()), e));
    }

    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::io(
            format!("could not replace {}", path.display()),
            e,
        ));
    }
    Ok(())
}

fn temp_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_creating_the_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested").join("file.toml");
        write(&path, "hello\n").expect("writes");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "hello\n");
    }

    #[test]
    fn replaces_rather_than_truncating_and_leaves_no_temp_files() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("file.toml");
        write(&path, "first\n").expect("writes");
        let before = std::fs::metadata(&path).expect("stat").ino();
        write(&path, "second\n").expect("writes again");

        assert_ne!(before, std::fs::metadata(&path).expect("stat").ino());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "second\n");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "file.toml")
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn a_directory_in_the_way_is_a_clean_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("file.toml");
        std::fs::create_dir(&path).expect("a directory where the file should go");
        assert!(matches!(write(&path, "x\n"), Err(Error::Io { .. })));
    }
}
