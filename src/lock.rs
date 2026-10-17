//! Exclusive per-cache run lock. Commands that write the cache (snapshots,
//! `state.toml`, `origins.toml`, the git clones, staged files) hold it for
//! their whole run, so two brewsoak runs never interleave.
//!
//! The lock is an advisory `flock` on `<cache>/run.lock`. The kernel drops it
//! when the holder exits, however it dies, so a lock file left by a crashed
//! run never blocks the next one. The holder's pid and command line go in a
//! separate file, `<cache>/run.lock.holder`, written whole by temp + rename
//! (never truncated in place), only so a waiter can name the holder.

use crate::Error;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = "run.lock";
const HOLDER_FILE: &str = "run.lock.holder";
/// How long a run waits for another run before giving up.
pub const WAIT: Duration = Duration::from_secs(5);
const RETRY_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct RunLock {
    file: File,
    holder: PathBuf,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // Remove the record while still holding the lock, so a waiter never
        // reads the record of a run that has finished.
        let _ = std::fs::remove_file(&self.holder);
        let _ = self.file.unlock();
    }
}

impl RunLock {
    /// A lock that guards nothing, for tests of code that only needs proof
    /// that its caller took the run lock.
    #[cfg(test)]
    pub fn for_tests() -> RunLock {
        RunLock {
            file: tempfile::tempfile().expect("temp file"),
            holder: PathBuf::from("/nonexistent/run.lock.holder"),
        }
    }
}

/// The run lock of one dispatch: either taken up front (commands that always
/// write the cache) or taken on demand by the code that finds it must write.
/// One process never locks twice: `get` reuses the lock it already holds.
pub struct LockSlot {
    cache: PathBuf,
    args: Vec<String>,
    wait: Duration,
    held: Option<RunLock>,
}

impl LockSlot {
    pub fn new(cache: &Path, args: &[String], wait: Duration) -> LockSlot {
        LockSlot {
            cache: cache.to_path_buf(),
            args: args.to_vec(),
            wait,
            held: None,
        }
    }

    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// The run lock, taken now (waiting up to the slot's limit) if this
    /// process does not hold it yet.
    pub fn get(&mut self) -> Result<&RunLock, Error> {
        if self.held.is_none() {
            self.held = Some(acquire_waiting(&self.cache, &self.args, self.wait)?);
        }
        Ok(self.held.as_ref().expect("lock just taken"))
    }
}

pub fn lock_path(cache: &Path) -> PathBuf {
    cache.join(LOCK_FILE)
}

pub fn holder_path(cache: &Path) -> PathBuf {
    cache.join(HOLDER_FILE)
}

/// Takes the cache lock for `args` (brewsoak's argv without the program name),
/// retrying for up to `wait` while another run holds it.
pub fn acquire_waiting(cache: &Path, args: &[String], wait: Duration) -> Result<RunLock, Error> {
    std::fs::create_dir_all(cache)?;
    let path = lock_path(cache);
    let file = OpenOptions::new()
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
                    return Err(Error::Refusal(busy_message(cache, &path)));
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
    let holder = holder_path(cache);
    // Drop a dead run's record first, so a waiter never names the wrong pid.
    let _ = std::fs::remove_file(&holder);
    crate::atomic::write(
        &holder,
        &format!("pid={}\ncommand={command}\n", std::process::id()),
    )?;
    Ok(RunLock { file, holder })
}

fn busy_message(cache: &Path, path: &Path) -> String {
    let record = std::fs::read_to_string(holder_path(cache)).unwrap_or_default();
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
        let record = std::fs::read_to_string(holder_path(dir.path())).unwrap();
        assert!(record.contains("command=brewsoak upgrade"), "{record}");
        drop(again);
    }

    #[test]
    fn a_stale_lock_file_from_a_dead_run_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        // A crashed run leaves its record behind but holds no flock.
        std::fs::write(
            holder_path(dir.path()),
            "pid=999999\ncommand=brewsoak upgrade --greedy\n",
        )
        .unwrap();
        let held = acquire_waiting(dir.path(), &args("update"), SHORT).expect("not blocked");
        let record = std::fs::read_to_string(holder_path(dir.path())).unwrap();
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
        std::fs::write(holder_path(dir.path()), "").unwrap();
        let text = acquire_waiting(dir.path(), &args("outdated"), SHORT)
            .unwrap_err()
            .to_string();
        assert!(text.contains("another brewsoak run"), "{text}");
        assert!(text.contains("pid is not recorded"), "{text}");
    }

    #[test]
    fn the_holder_record_lives_in_its_own_file_and_is_never_seen_empty() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire_waiting(dir.path(), &args("update"), SHORT).unwrap();
        // The flock file itself carries no record to truncate.
        assert_eq!(std::fs::read_to_string(lock_path(dir.path())).unwrap(), "");
        let record = std::fs::read_to_string(holder_path(dir.path())).unwrap();
        assert!(record.contains("command=brewsoak update"), "{record}");
        drop(held);
        assert!(!holder_path(dir.path()).exists(), "record outlived the run");

        // A reader polling while runs come and go sees a whole record or none.
        let path = holder_path(dir.path());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let reader = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    assert!(
                        text.contains("pid=") && text.ends_with('\n'),
                        "partial record: {text:?}"
                    );
                }
            }
        });
        for _ in 0..200 {
            drop(acquire_waiting(dir.path(), &args("update"), SHORT).unwrap());
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap();
    }

    #[test]
    fn a_lock_slot_takes_the_lock_once_and_blocks_others_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let mut slot = LockSlot::new(dir.path(), &args("outdated"), SHORT);
        assert!(!slot.is_held());
        // Nothing is locked until `get`.
        drop(acquire_waiting(dir.path(), &args("update"), SHORT).unwrap());
        slot.get().expect("first get locks");
        slot.get()
            .expect("second get reuses the lock, never blocks itself");
        assert!(slot.is_held());
        let err = acquire_waiting(dir.path(), &args("update"), SHORT).unwrap_err();
        assert!(err.to_string().contains("`brewsoak outdated`"), "{err}");
        drop(slot);
        acquire_waiting(dir.path(), &args("update"), SHORT).expect("released with the slot");
    }
}
