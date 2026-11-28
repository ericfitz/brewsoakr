//! Whole-file replacement for brewsoak's cache files (`state.toml`,
//! `origins.toml`): write a private temp file, fsync it, rename it over the
//! target, fsync the directory. A reader sees the old file or the new one, never a partial write,
//! and a crash leaves at worst a stale temp file that the next write sweeps.

use crate::Error;
use crate::config::{remove_stale_temps, write_temp};
use std::path::Path;

pub fn write(path: &Path, body: &str) -> Result<(), Error> {
    write_bytes(path, body.as_bytes())
}

pub fn write_bytes(path: &Path, body: &[u8]) -> Result<(), Error> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| Error::Other(format!("{} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Other(format!("{} has no file name", path.display())))?;
    create_dirs_durably(dir)?;
    let prefix = format!(".{name}.tmp-");
    let tmp = dir.join(format!("{prefix}{}", std::process::id()));
    let result = write_temp(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    // Make the rename itself durable: without this a power loss right after
    // the rename can bring back the old file.
    sync_dir(dir)?;
    remove_stale_temps(dir, &prefix, &tmp)
}

/// `create_dir_all`, then an fsync of the parent of every directory this call
/// created, so a power loss cannot lose the new directory entry (and the file
/// renamed into it) while the file's own directory fsync succeeded.
fn create_dirs_durably(dir: &Path) -> Result<(), Error> {
    let mut missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.exists()).collect();
    std::fs::create_dir_all(dir)?;
    missing.reverse();
    for created in missing {
        if let Some(parent) = created.parent() {
            sync_dir(dir_or_cwd(parent))?;
        }
    }
    Ok(())
}

/// The parent of a relative one-component path is empty: that is the current
/// directory, which still needs its fsync.
fn dir_or_cwd(dir: &Path) -> &Path {
    if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    }
}

fn sync_dir(dir: &Path) -> Result<(), Error> {
    #[cfg(test)]
    SYNCED.with(|s| s.borrow_mut().push(dir.to_path_buf()));
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Every directory `sync_dir` was asked to fsync on this thread (test seam).
    static SYNCED: std::cell::RefCell<Vec<std::path::PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn new_directories_are_synced_into_their_parents() {
        // Everything stays under a temp dir. A relative one-component path
        // (whose parent is the empty path) needs the process cwd, which a
        // parallel test must not change, so that case is `dir_or_cwd` below.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("b");
        SYNCED.with(|s| s.borrow_mut().clear());
        create_dirs_durably(&dir).unwrap();
        let synced = SYNCED.with(|s| s.borrow().clone());
        assert_eq!(synced, vec![tmp.path().to_path_buf(), tmp.path().join("a")]);
        assert_eq!(dir_or_cwd(Path::new("")), Path::new("."));
        assert_eq!(dir_or_cwd(Path::new("a")), Path::new("a"));
    }

    #[test]
    fn replaces_the_file_wholesale_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        write(&path, &"a = 1\n".repeat(1000)).unwrap();
        write(&path, "b = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b = 2\n");
        assert_eq!(names(dir.path()), vec!["state.toml".to_string()]);
    }

    #[test]
    fn a_failed_write_leaves_the_old_file_and_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        write(&path, "old = true\n").unwrap();
        // Renaming a file over a directory fails.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write(&blocked, "new = true\n").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old = true\n");
        assert_eq!(
            names(dir.path()),
            vec!["blocked".to_string(), "state.toml".to_string()]
        );
    }

    #[test]
    fn sweeps_a_stale_temp_from_a_dead_writer_but_not_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("origins.toml");
        let stale = dir.path().join(".origins.toml.tmp-1");
        let fresh = dir.path().join(".origins.toml.tmp-2");
        std::fs::write(&stale, "partial").unwrap();
        std::fs::write(&fresh, "in flight").unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        write(&path, "x = 1\n").unwrap();
        assert_eq!(
            names(dir.path()),
            vec![
                ".origins.toml.tmp-2".to_string(),
                "origins.toml".to_string()
            ]
        );
    }

    #[test]
    fn syncs_the_directory_after_the_rename() {
        let dir = tempfile::tempdir().unwrap();
        sync_dir(dir.path()).expect("a directory can be fsynced");
        assert!(sync_dir(&dir.path().join("missing")).is_err());
        let nested = dir.path().join("a/b/state.toml");
        write(&nested, "x = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "x = 1\n");
    }

    #[test]
    fn fsyncs_the_parent_of_each_directory_it_creates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        SYNCED.with(|s| s.borrow_mut().clear());
        write(&root.join("a/b/c/state.toml"), "x = 1\n").unwrap();
        let synced = SYNCED.with(|s| s.borrow().clone());
        // `a` existed; `b` and `c` are new: `a` and `a/b` each get an fsync,
        // then the file's directory `a/b/c` after the rename.
        assert_eq!(
            synced,
            vec![root.join("a"), root.join("a/b"), root.join("a/b/c")]
        );
        // A second write creates nothing, so only the file's directory syncs.
        SYNCED.with(|s| s.borrow_mut().clear());
        write(&root.join("a/b/c/state.toml"), "x = 2\n").unwrap();
        assert_eq!(
            SYNCED.with(|s| s.borrow().clone()),
            vec![root.join("a/b/c")]
        );
    }

    #[test]
    fn write_bytes_round_trips_non_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Formula/x.rb");
        let body = b"a\r\n\xff\xfe\x00b";
        write_bytes(&path, body).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }
}
