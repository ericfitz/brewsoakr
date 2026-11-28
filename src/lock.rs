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

/// brew's own lock on one formula (`FormulaLock`: an `flock` on
/// `<locks>/<name>.formula.lock`), held until dropped. The file is left in
/// place, as brew leaves it.
#[derive(Debug)]
pub struct BrewLock {
    _file: Option<File>,
}

impl BrewLock {
    /// A lock that guards nothing, for a `Brew` double.
    pub fn unguarded() -> BrewLock {
        BrewLock { _file: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrewLockError {
    /// Another process holds it: brew is working on the formula.
    Busy,
    /// The lock could not be taken for another reason.
    Unavailable(String),
}

/// Takes brew's lock on formula `name` in `locks_dir` without waiting.
pub fn try_brew_formula_lock(locks_dir: &Path, name: &str) -> Result<BrewLock, BrewLockError> {
    lock_with_hook(locks_dir, name, &|_, _| {})
}

/// `try_brew_formula_lock`, with `between(path, attempt)` called after each
/// lock and before the identity check: the window brew's `cleanup` can unlink
/// the file in. Tests use it to play brew.
fn lock_with_hook(
    locks_dir: &Path,
    name: &str,
    between: &dyn Fn(&Path, usize),
) -> Result<BrewLock, BrewLockError> {
    use std::os::unix::fs::MetadataExt;
    let unavailable = BrewLockError::Unavailable;
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(unavailable(format!("{name:?} is not a formula name")));
    }
    std::fs::create_dir_all(locks_dir).map_err(|e| unavailable(e.to_string()))?;
    let path = locks_dir.join(format!("{name}.formula.lock"));
    // brew unlinks lock files in `brew cleanup`, so the file that was locked
    // must still be the one at the path (brew's own check, `lock_file.rb`).
    // The path is judged by `symlink_metadata`, so a link swapped in after the
    // first check is a different file and sends the loop around to refuse it.
    for attempt in 0..3 {
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(unavailable("the lock file is a symbolic link".into()));
        }
        // Open read-only first: it creates and writes nothing, and a link
        // swapped in now is only read through. A missing file is created with
        // O_EXCL, which never follows a link (a dangling one gives
        // AlreadyExists and the loop refuses it on the next pass).
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(unavailable(e.to_string())),
                }
            }
            Err(e) => return Err(unavailable(e.to_string())),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(BrewLockError::Busy),
            Err(TryLockError::Error(e)) => return Err(unavailable(e.to_string())),
        }
        between(&path, attempt);
        let same = match (file.metadata(), std::fs::symlink_metadata(&path)) {
            (Ok(a), Ok(b)) => {
                !b.file_type().is_symlink() && a.ino() == b.ino() && a.dev() == b.dev()
            }
            _ => false,
        };
        if same {
            return Ok(BrewLock { _file: Some(file) });
        }
    }
    Err(unavailable("lock file kept changing".into()))
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

    #[test]
    fn brew_formula_lock_is_busy_while_another_handle_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let held = try_brew_formula_lock(dir.path(), "wget").unwrap();
        assert_eq!(
            try_brew_formula_lock(dir.path(), "wget").unwrap_err(),
            BrewLockError::Busy
        );
        assert!(try_brew_formula_lock(dir.path(), "curl").is_ok());
        drop(held);
        assert!(try_brew_formula_lock(dir.path(), "wget").is_ok());
    }

    #[test]
    fn brew_formula_lock_is_busy_against_a_plain_flock_on_brews_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.path().join("wget.formula.lock"))
            .unwrap();
        file.try_lock().unwrap();
        assert_eq!(
            try_brew_formula_lock(dir.path(), "wget").unwrap_err(),
            BrewLockError::Busy
        );
        file.unlock().unwrap();
        assert!(try_brew_formula_lock(dir.path(), "wget").is_ok());
    }

    #[test]
    fn a_stale_lock_file_from_a_crashed_brew_does_not_block() {
        // flock dies with its process: a file left behind with no holder is free.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("wget.formula.lock"), b"").unwrap();
        assert!(try_brew_formula_lock(dir.path(), "wget").is_ok());
    }

    #[test]
    fn brew_formula_lock_creates_the_directory_and_leaves_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("var/homebrew/locks");
        let guard = try_brew_formula_lock(&dir, "wget").unwrap();
        drop(guard);
        let meta = std::fs::metadata(dir.join("wget.formula.lock")).unwrap();
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn brew_formula_lock_refuses_a_symbolic_link() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("target"),
            tmp.path().join("wget.formula.lock"),
        )
        .unwrap();
        assert_eq!(
            try_brew_formula_lock(tmp.path(), "wget").unwrap_err(),
            BrewLockError::Unavailable("the lock file is a symbolic link".into())
        );
        assert!(!tmp.path().join("target").exists());
    }

    #[test]
    fn brew_formula_lock_rejects_names_that_are_not_file_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("locks");
        for name in ["", "a/b", "..", "a\0b"] {
            let err = try_brew_formula_lock(&dir, name).unwrap_err();
            assert!(matches!(err, BrewLockError::Unavailable(_)), "{name:?}");
        }
        assert!(!dir.exists(), "nothing is created for a refused name");
    }

    #[test]
    fn brew_formula_lock_reports_a_directory_it_cannot_create() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"x").unwrap();
        let err = try_brew_formula_lock(&tmp.path().join("file/locks"), "wget").unwrap_err();
        assert!(matches!(err, BrewLockError::Unavailable(_)), "{err:?}");
    }

    #[test]
    fn brew_formula_lock_takes_odd_names_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let _a = try_brew_formula_lock(dir.path(), "python@3.12").unwrap();
        let _b = try_brew_formula_lock(dir.path(), "c++filt").unwrap();
        assert!(dir.path().join("python@3.12.formula.lock").exists());
        assert!(dir.path().join("c++filt.formula.lock").exists());
    }

    #[test]
    fn brew_formula_lock_refuses_a_dangling_link_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::os::unix::fs::symlink(&target, tmp.path().join("wget.formula.lock")).unwrap();
        let err = try_brew_formula_lock(tmp.path(), "wget").unwrap_err();
        assert_eq!(
            err,
            BrewLockError::Unavailable("the lock file is a symbolic link".into())
        );
        assert!(target.symlink_metadata().is_err(), "target not created");
    }

    #[test]
    fn brew_formula_lock_retries_when_brew_unlinks_the_file_after_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let unlinks = std::cell::Cell::new(0);
        let got = lock_with_hook(dir.path(), "wget", &|path, attempt| {
            if attempt == 0 {
                std::fs::remove_file(path).unwrap();
                unlinks.set(unlinks.get() + 1);
            }
        });
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(unlinks.get(), 1);
        assert!(dir.path().join("wget.formula.lock").exists());
    }

    #[test]
    fn brew_formula_lock_gives_up_loudly_when_the_file_changes_every_time() {
        let dir = tempfile::tempdir().unwrap();
        let got = lock_with_hook(dir.path(), "wget", &|path, _| {
            std::fs::remove_file(path).unwrap();
        });
        assert_eq!(
            got.unwrap_err(),
            BrewLockError::Unavailable("lock file kept changing".into())
        );
    }

    #[test]
    fn unguarded_lock_guards_nothing() {
        drop(BrewLock::unguarded());
    }
}
