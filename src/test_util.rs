//! Helpers shared by unit tests in several modules.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Write `contents` to `path` and make it executable (a fake `brew` script).
pub(crate) fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::write(path, contents).expect("script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(test)]
mod tests {
    use super::*;

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
