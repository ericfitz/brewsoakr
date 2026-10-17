//! Exclusive per-cache run lock. Commands that write the cache (snapshots,
//! `state.toml`, `origins.toml`, the git clones, staged files) hold it for
//! their whole run, so two brewsoak runs never interleave.
//!
//! The lock is an advisory `flock` on `<cache>/run.lock`. The kernel drops it
//! when the holder exits, however it dies, so a lock file left by a crashed
//! run never blocks the next one. The holder's pid and command line are
//! written into the file only so a waiter can name it.

use crate::Error;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = "run.lock";
/// How long a run waits for another run before giving up.
pub const WAIT: Duration = Duration::from_secs(5);
const RETRY_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct RunLock {
    file: File,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn lock_path(cache: &Path) -> PathBuf {
    cache.join(LOCK_FILE)
}

/// Takes the cache lock for `args` (brewsoak's argv without the program name),
/// retrying for up to `wait` while another run holds it.
pub fn acquire_waiting(cache: &Path, args: &[String], wait: Duration) -> Result<RunLock, Error> {
    std::fs::create_dir_all(cache)?;
    let path = lock_path(cache);
    // No truncate: a waiter reads the holder's record out of this file.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                if started.elapsed() >= wait {
                    return Err(Error::Refusal(busy_message(&mut file, &path)));
                }
                std::thread::sleep(RETRY_EVERY);
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    let command = std::iter::once("brewsoak")
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['\n', '\r'], " ");
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "pid={}\ncommand={command}\n", std::process::id())?;
    file.flush()?;
    Ok(RunLock { file })
}

fn busy_message(file: &mut File, path: &Path) -> String {
    let mut record = String::new();
    let _ = file.rewind();
    let _ = file.read_to_string(&mut record);
    let field = |key: &str| {
        record
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let holder = match (field("pid"), field("command")) {
        (Some(pid), Some(command)) => format!("pid {pid}, `{command}`"),
        (Some(pid), None) => format!("pid {pid}"),
        _ => "its pid is not recorded".to_string(),
    };
    format!(
        "another brewsoak run is using the cache ({holder}); wait for it to finish and run \
         this command again (lock file: {})",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    const SHORT: Duration = Duration::from_millis(150);

    #[test]
    fn a_second_lock_fails_naming_the_holders_pid_and_command() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire_waiting(dir.path(), &args("upgrade jq"), SHORT).unwrap();
        let err = acquire_waiting(dir.path(), &args("outdated"), SHORT).unwrap_err();
        assert!(matches!(err, Error::Refusal(_)), "{err}");
        let text = err.to_string();
        assert!(
            text.contains(&format!("pid {}", std::process::id())),
            "{text}"
        );
        assert!(text.contains("`brewsoak upgrade jq`"), "{text}");
        assert!(text.contains("run.lock"), "{text}");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn a_second_lock_waits_for_the_holder_to_finish() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire_waiting(dir.path(), &args("update"), SHORT).unwrap();
        let path = dir.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            acquire_waiting(&path, &args("outdated"), Duration::from_secs(10)).map(|_| ())
        });
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
        waiter.join().unwrap().expect("acquired once released");
    }

    #[test]
    fn the_lock_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        drop(acquire_waiting(dir.path(), &args("update"), SHORT).unwrap());
        let again = acquire_waiting(dir.path(), &args("upgrade"), SHORT).unwrap();
        let record = std::fs::read_to_string(lock_path(dir.path())).unwrap();
        assert!(record.contains("command=brewsoak upgrade"), "{record}");
        drop(again);
    }

    #[test]
    fn a_stale_lock_file_from_a_dead_run_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        // A crashed run leaves its record behind but holds no flock.
        std::fs::write(
            lock_path(dir.path()),
            "pid=999999\ncommand=brewsoak upgrade --greedy\n",
        )
        .unwrap();
        let held = acquire_waiting(dir.path(), &args("update"), SHORT).expect("not blocked");
        let record = std::fs::read_to_string(lock_path(dir.path())).unwrap();
        assert!(
            record.contains(&format!("pid={}", std::process::id())),
            "{record}"
        );
        assert!(
            !record.contains("999999"),
            "stale record replaced: {record}"
        );
        assert!(!record.contains("--greedy"), "{record}");
        drop(held);
    }

    #[test]
    fn a_lock_file_with_no_record_still_gives_an_actionable_message() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire_waiting(dir.path(), &args("update"), SHORT).unwrap();
        std::fs::write(lock_path(dir.path()), "").unwrap();
        let text = acquire_waiting(dir.path(), &args("outdated"), SHORT)
            .unwrap_err()
            .to_string();
        assert!(text.contains("another brewsoak run"), "{text}");
        assert!(text.contains("pid is not recorded"), "{text}");
    }
}
