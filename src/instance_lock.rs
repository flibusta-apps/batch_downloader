use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
};

/// The OS releases this exclusive lock on exit/crash. Never unlink the lock
/// file: all instances must lock the same inode, including during cleanup.
pub fn acquire(dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".instance.lock"))?;
    file.try_lock().map_err(io::Error::other)?;
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    #[test]
    fn exclusive_until_guard_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let guard = super::acquire(dir.path()).unwrap();
        assert!(super::acquire(dir.path()).is_err());
        drop(guard);
        assert!(super::acquire(dir.path()).is_ok());
    }
}
