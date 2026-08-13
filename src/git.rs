use crate::Error;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub const REF_CUTOFF: &str = "refs/brewsoak/cutoff";
pub const REF_HEAD: &str = "refs/brewsoak/head";
pub const REF_WINDOW: &str = "refs/brewsoak/window";

pub trait GitStore {
    fn init_bare(&self, dir: &Path) -> Result<(), Error>;
    fn fetch_depth1(
        &self,
        dir: &Path,
        remote: &str,
        sha: &str,
        ref_name: &str,
    ) -> Result<(), Error>;
    /// Returns `None` if the path is missing (git exit 128 / “does not exist”).
    fn show(&self, dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error>;
    fn rev_parse(&self, dir: &Path, rev: &str) -> Result<Option<String>, Error>;
    fn gc_prune(&self, dir: &Path) -> Result<(), Error>;
    /// Fetch recent history into `refs/brewsoak/window`. Default: unsupported.
    fn fetch_shallow_since(
        &self,
        _dir: &Path,
        _remote: &str,
        _until_unix: i64,
    ) -> Result<(), Error> {
        Err(Error::Git {
            action: "fetching history to find the soak cutoff".into(),
            detail: "this git backend cannot shallow-fetch".into(),
        })
    }
    /// SHA of the latest commit on `refs/brewsoak/window` at or before `until_unix`.
    fn log_sha_before(&self, _dir: &Path, _until_unix: i64) -> Result<Option<String>, Error> {
        Ok(None)
    }
}

pub struct ProcessGit;

impl ProcessGit {
    fn git() -> Command {
        let mut cmd = Command::new("git");
        cmd.stdin(Stdio::null());
        cmd
    }
}

fn git_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    format!("git exited {}", output.status)
}

fn git_fail(action: &str, output: &Output) -> Error {
    Error::Git {
        action: action.to_string(),
        detail: git_detail(output),
    }
}

fn run_git(action: &str, args: &[&str]) -> Result<Output, Error> {
    ProcessGit::git()
        .args(args)
        .output()
        .map_err(|e| Error::Git {
            action: action.to_string(),
            detail: format!("could not start git: {e}"),
        })
}

fn missing_object(output: &Output) -> bool {
    output.status.code() == Some(128)
        || String::from_utf8_lossy(&output.stderr).contains("does not exist")
}

impl GitStore for ProcessGit {
    fn init_bare(&self, dir: &Path) -> Result<(), Error> {
        let action = "creating the local soak git store";
        let dir = dir.to_string_lossy();
        let output = run_git(action, &["init", "--bare", dir.as_ref()])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail(action, &output))
        }
    }

    fn fetch_depth1(
        &self,
        dir: &Path,
        remote: &str,
        sha: &str,
        ref_name: &str,
    ) -> Result<(), Error> {
        // Pins are not ancestry-ordered. Force so a later cutoff/HEAD SHA
        // that is older or disconnected (depth-1) still replaces the pin.
        let spec = format!("+{sha}:{ref_name}");
        let action = format!("updating soak pin {ref_name} to {sha} from {remote}");
        let dir = dir.to_string_lossy();
        let output = run_git(
            &action,
            &[
                "--git-dir",
                dir.as_ref(),
                "fetch",
                "--force",
                "--depth=1",
                remote,
                &spec,
            ],
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail(&action, &output))
        }
    }

    fn show(&self, dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error> {
        let spec = format!("{sha}:{path}");
        let action = format!("reading {path} from git commit {sha}");
        let dir = dir.to_string_lossy();
        let output = run_git(&action, &["--git-dir", dir.as_ref(), "show", &spec])?;
        if output.status.success() {
            Ok(Some(output.stdout))
        } else if missing_object(&output) {
            Ok(None)
        } else {
            Err(git_fail(&action, &output))
        }
    }

    fn rev_parse(&self, dir: &Path, rev: &str) -> Result<Option<String>, Error> {
        let action = format!("resolving git ref {rev}");
        let dir = dir.to_string_lossy();
        let output = run_git(&action, &["--git-dir", dir.as_ref(), "rev-parse", rev])?;
        if output.status.success() {
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(Some(sha))
        } else if missing_object(&output) {
            Ok(None)
        } else {
            Err(git_fail(&action, &output))
        }
    }

    fn gc_prune(&self, dir: &Path) -> Result<(), Error> {
        let action = "pruning unused objects from the soak git cache";
        let dir = dir.to_string_lossy();
        let output = run_git(action, &["-C", dir.as_ref(), "gc", "--prune=now"])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail(action, &output))
        }
    }

    fn fetch_shallow_since(&self, dir: &Path, remote: &str, until_unix: i64) -> Result<(), Error> {
        let action = format!("fetching history from {remote} to find the soak cutoff");
        let dir = dir.to_string_lossy();
        let since = format!("--shallow-since={until_unix}");
        let spec = format!("+HEAD:{REF_WINDOW}");
        let output = run_git(
            &action,
            &[
                "--git-dir",
                dir.as_ref(),
                "fetch",
                "--force",
                &since,
                remote,
                &spec,
            ],
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail(&action, &output))
        }
    }

    fn log_sha_before(&self, dir: &Path, until_unix: i64) -> Result<Option<String>, Error> {
        let action = "looking up the last commit at or before the soak cutoff";
        let dir = dir.to_string_lossy();
        let before = format!("--before={until_unix}");
        let output = run_git(
            action,
            &[
                "--git-dir",
                dir.as_ref(),
                "log",
                "-1",
                &before,
                "--format=%H",
                REF_WINDOW,
            ],
        )?;
        if output.status.success() {
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if sha.is_empty() {
                Ok(None)
            } else {
                Ok(Some(sha))
            }
        } else if missing_object(&output) {
            Ok(None)
        } else {
            Err(git_fail(action, &output))
        }
    }
}

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
#[derive(Default)]
pub struct InMemoryGit {
    blobs: RefCell<HashMap<(String, String), Vec<u8>>>,
    refs: RefCell<HashMap<String, String>>,
    fetched: RefCell<Vec<(String, String)>>,
}

#[cfg(test)]
impl InMemoryGit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_blob(&self, sha: &str, path: &str, bytes: impl Into<Vec<u8>>) {
        self.blobs
            .borrow_mut()
            .insert((sha.to_string(), path.to_string()), bytes.into());
    }

    pub fn fetched(&self) -> Vec<(String, String)> {
        self.fetched.borrow().clone()
    }
}

#[cfg(test)]
impl GitStore for InMemoryGit {
    fn init_bare(&self, _dir: &Path) -> Result<(), Error> {
        Ok(())
    }

    fn fetch_depth1(
        &self,
        _dir: &Path,
        _remote: &str,
        sha: &str,
        ref_name: &str,
    ) -> Result<(), Error> {
        self.fetched
            .borrow_mut()
            .push((sha.to_string(), ref_name.to_string()));
        self.refs
            .borrow_mut()
            .insert(ref_name.to_string(), sha.to_string());
        Ok(())
    }

    fn show(&self, _dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self
            .blobs
            .borrow()
            .get(&(sha.to_string(), path.to_string()))
            .cloned())
    }

    fn rev_parse(&self, _dir: &Path, rev: &str) -> Result<Option<String>, Error> {
        Ok(self.refs.borrow().get(rev).cloned())
    }

    fn gc_prune(&self, _dir: &Path) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn unused_dir() -> &'static Path {
        Path::new("/brewsoak-in-memory-unused")
    }

    #[test]
    fn show_missing_path_is_none() {
        let git = InMemoryGit::new();
        let got = git
            .show(unused_dir(), "abc123", "Formula/w/wget.rb")
            .expect("show");
        assert_eq!(got, None);
    }

    #[test]
    fn show_present_returns_bytes() {
        let git = InMemoryGit::new();
        git.insert_blob("abc123", "Formula/w/wget.rb", b"class Wget < Formula\n");
        let got = git
            .show(unused_dir(), "abc123", "Formula/w/wget.rb")
            .expect("show");
        assert_eq!(got.as_deref(), Some(b"class Wget < Formula\n".as_slice()));
    }

    #[test]
    fn fetch_records_ref_and_rev_parse() {
        let git = InMemoryGit::new();
        git.fetch_depth1(
            unused_dir(),
            "https://github.com/Homebrew/homebrew-core",
            "cutoffsha1",
            REF_CUTOFF,
        )
        .expect("fetch");
        assert_eq!(
            git.fetched(),
            vec![("cutoffsha1".into(), REF_CUTOFF.into())]
        );
        assert_eq!(
            git.rev_parse(unused_dir(), REF_CUTOFF).expect("rev-parse"),
            Some("cutoffsha1".into())
        );
    }

    #[test]
    fn fetch_replaces_cutoff_sha() {
        let git = InMemoryGit::new();
        git.fetch_depth1(
            unused_dir(),
            "https://github.com/Homebrew/homebrew-core",
            "oldcutoff",
            REF_CUTOFF,
        )
        .expect("fetch old");
        git.fetch_depth1(
            unused_dir(),
            "https://github.com/Homebrew/homebrew-core",
            "newcutoff",
            REF_CUTOFF,
        )
        .expect("fetch new");
        assert_eq!(
            git.rev_parse(unused_dir(), REF_CUTOFF).expect("rev-parse"),
            Some("newcutoff".into())
        );
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn process_git_fetch_replaces_non_fast_forward_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bare = tmp.path().join("bare.git");
        std::fs::create_dir(&src).unwrap();
        git_ok(&src, &["init", "-b", "main"]);
        git_ok(&src, &["config", "user.email", "test@example.com"]);
        git_ok(&src, &["config", "user.name", "Test"]);
        std::fs::write(src.join("f"), "one\n").unwrap();
        git_ok(&src, &["add", "f"]);
        git_ok(&src, &["commit", "-m", "one"]);
        let older = git_ok(&src, &["rev-parse", "HEAD"]);
        std::fs::write(src.join("f"), "two\n").unwrap();
        git_ok(&src, &["add", "f"]);
        git_ok(&src, &["commit", "-m", "two"]);
        let newer = git_ok(&src, &["rev-parse", "HEAD"]);

        let git = ProcessGit;
        git.init_bare(&bare).expect("init bare");
        git.fetch_depth1(&bare, src.to_str().unwrap(), &newer, REF_CUTOFF)
            .expect("fetch newer");
        git.fetch_depth1(&bare, src.to_str().unwrap(), &older, REF_CUTOFF)
            .expect("fetch older (non-fast-forward)");
        assert_eq!(
            git.rev_parse(&bare, REF_CUTOFF).expect("rev-parse"),
            Some(older)
        );
    }

    #[test]
    fn process_git_shallow_window_replaces_non_fast_forward() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bare = tmp.path().join("bare.git");
        std::fs::create_dir(&src).unwrap();
        git_ok(&src, &["init", "-b", "main"]);
        git_ok(&src, &["config", "user.email", "test@example.com"]);
        git_ok(&src, &["config", "user.name", "Test"]);
        std::fs::write(src.join("f"), "one\n").unwrap();
        git_ok(&src, &["add", "f"]);
        git_ok(&src, &["commit", "-m", "one"]);
        let older = git_ok(&src, &["rev-parse", "HEAD"]);
        std::fs::write(src.join("f"), "two\n").unwrap();
        git_ok(&src, &["add", "f"]);
        git_ok(&src, &["commit", "-m", "two"]);
        let newer = git_ok(&src, &["rev-parse", "HEAD"]);

        let git = ProcessGit;
        git.init_bare(&bare).expect("init bare");
        git.fetch_depth1(&bare, src.to_str().unwrap(), &newer, REF_WINDOW)
            .expect("pin window to newer");
        git_ok(&src, &["reset", "--hard", &older]);
        git.fetch_shallow_since(&bare, src.to_str().unwrap(), 0)
            .expect("shallow fetch older HEAD onto window");
        assert_eq!(
            git.rev_parse(&bare, REF_WINDOW).expect("rev-parse"),
            Some(older)
        );
    }

    #[test]
    fn process_git_fetch_error_explains_action_and_includes_git_output() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare.git");
        let git = ProcessGit;
        git.init_bare(&bare).expect("init bare");
        let err = git
            .fetch_depth1(
                &bare,
                "/no/such/brewsoak-remote",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                REF_CUTOFF,
            )
            .expect_err("fetch must fail");
        let text = err.to_string();
        assert!(text.contains("updating soak pin"), "missing action: {text}");
        assert!(
            text.contains("git failed"),
            "missing brewsoakr framing: {text}"
        );
        assert!(
            text.to_lowercase()
                .contains("does not appear to be a git repository")
                || text.contains("fatal:"),
            "missing git output: {text}"
        );
    }
}
