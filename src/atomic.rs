//! Whole-file replacement for brewsoak's cache files (`state.toml`,
//! `origins.toml`): write a private temp file, fsync it, rename it over the
//! target. A reader sees the old file or the new one, never a partial write,
//! and a crash leaves at worst a stale temp file that the next write sweeps.

use crate::Error;
use crate::config::{remove_stale_temps, write_temp};
use std::path::Path;

pub fn write(path: &Path, body: &str) -> Result<(), Error> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| Error::Other(format!("{} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Other(format!("{} has no file name", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let prefix = format!(".{name}.tmp-");
    let tmp = dir.join(format!("{prefix}{}", std::process::id()));
    let result = write_temp(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    remove_stale_temps(dir, &prefix, &tmp)
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
}
