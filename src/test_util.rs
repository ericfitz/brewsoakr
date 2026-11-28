//! Helpers shared by unit tests in several modules.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Write `contents` to `path` and make it executable (a fake `brew` script).
pub(crate) fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::write(path, contents).expect("script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Takes brew's formula lock, retrying for up to 500 ms. A test that re-locks
/// right after a release can see the old flock still held: another test's
/// fork shares the open file description until its child calls exec, and
/// `O_CLOEXEC` only closes it then. Production is unaffected, because
/// brewsoak never spawns a child while it holds a brew lock.
pub(crate) fn lock_eventually(
    locks_dir: &Path,
    name: &str,
) -> Result<crate::lock::BrewLock, crate::lock::BrewLockError> {
    lock_eventually_with(|| crate::lock::try_brew_formula_lock(locks_dir, name))
}

/// The retry loop of `lock_eventually` over any lock attempt. Only `Busy` is
/// retried.
pub(crate) fn lock_eventually_with(
    mut attempt: impl FnMut() -> Result<crate::lock::BrewLock, crate::lock::BrewLockError>,
) -> Result<crate::lock::BrewLock, crate::lock::BrewLockError> {
    for _ in 0..49 {
        match attempt() {
            Err(crate::lock::BrewLockError::Busy) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return other,
        }
    }
    attempt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_eventually_retries_busy_then_returns_the_answer() {
        let mut n = 0;
        let got = lock_eventually_with(|| {
            n += 1;
            if n < 3 {
                Err(crate::lock::BrewLockError::Busy)
            } else {
                Ok(crate::lock::BrewLock::unguarded())
            }
        });
        assert!(got.is_ok());
        assert_eq!(n, 3);
        let mut m = 0;
        let got = lock_eventually_with(|| {
            m += 1;
            Err(crate::lock::BrewLockError::Unavailable("x".into()))
        });
        assert!(got.is_err());
        assert_eq!(m, 1, "only Busy is retried");
    }

    #[test]
    fn write_executable_writes_the_bytes_and_sets_the_exec_bits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("brew");
        write_executable(&path, "#!/bin/sh\nexit 0\n");
        assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\nexit 0\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(
            std::process::Command::new(&path)
                .status()
                .unwrap()
                .success()
        );
    }
}
