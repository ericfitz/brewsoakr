use crate::Error;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub const REF_CUTOFF: &str = "refs/brewsoak/cutoff";
pub const REF_HEAD: &str = "refs/brewsoak/head";

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
}

pub struct ProcessGit;

impl ProcessGit {
    fn git() -> Command {
        let mut cmd = Command::new("git");
        cmd.stdin(Stdio::null());
        cmd
    }
}

fn git_fail(context: &str, output: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Error::Other(format!("{context}: git exited {}", output.status))
    } else {
        Error::Other(format!("{context}: {stderr}"))
    }
}

fn missing_object(output: &Output) -> bool {
    output.status.code() == Some(128)
        || String::from_utf8_lossy(&output.stderr).contains("does not exist")
}

impl GitStore for ProcessGit {
    fn init_bare(&self, dir: &Path) -> Result<(), Error> {
        let output = Self::git().arg("init").arg("--bare").arg(dir).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail("git init --bare", &output))
        }
    }

    fn fetch_depth1(
        &self,
        dir: &Path,
        remote: &str,
        sha: &str,
        ref_name: &str,
    ) -> Result<(), Error> {
        // Cutoff/HEAD pins are not ancestry-ordered: a later update may move
        // the ref to an older commit, or to a depth-1 object with no local
        // history connecting it to the previous pin. Force the update.
        let spec = format!("+{sha}:{ref_name}");
        let output = Self::git()
            .arg("--git-dir")
            .arg(dir)
            .arg("fetch")
            .arg("--depth=1")
            .arg(remote)
            .arg(&spec)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail("git fetch --depth=1", &output))
        }
    }

    fn show(&self, dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error> {
        let spec = format!("{sha}:{path}");
        let output = Self::git()
            .arg("--git-dir")
            .arg(dir)
            .arg("show")
            .arg(&spec)
            .output()?;
        if output.status.success() {
            Ok(Some(output.stdout))
        } else if missing_object(&output) {
            Ok(None)
        } else {
            Err(git_fail("git show", &output))
        }
    }

    fn rev_parse(&self, dir: &Path, rev: &str) -> Result<Option<String>, Error> {
        let output = Self::git()
            .arg("--git-dir")
            .arg(dir)
            .arg("rev-parse")
            .arg(rev)
            .output()?;
        if output.status.success() {
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(Some(sha))
        } else if missing_object(&output) {
            Ok(None)
        } else {
            Err(git_fail("git rev-parse", &output))
        }
    }

    fn gc_prune(&self, dir: &Path) -> Result<(), Error> {
        let output = Self::git()
            .arg("-C")
            .arg(dir)
            .arg("gc")
            .arg("--prune=now")
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail("git gc --prune=now", &output))
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
}
