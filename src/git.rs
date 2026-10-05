use crate::Error;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::rc::Rc;

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
    /// `git remote add <name> <url>`, or `set-url` when it exists.
    fn set_remote(&self, _dir: &Path, name: &str, url: &str) -> Result<(), Error> {
        Err(Error::Git {
            action: format!("registering tap remote {name} ({url})"),
            detail: "this git backend cannot add remotes".into(),
        })
    }
    /// Full commit history of the remote's HEAD into `ref_name` (force), blobless
    /// (`--filter=blob:none`); retried without the filter if the server rejects it.
    fn fetch_history(&self, _dir: &Path, remote_name: &str, _ref_name: &str) -> Result<(), Error> {
        Err(Error::Git {
            action: format!("fetching history from {remote_name}"),
            detail: "this git backend cannot fetch history".into(),
        })
    }
    /// `(sha, committer_unix)` of the newest commit reachable from `rev` with
    /// committer time <= `until_unix`; `None` when the history is younger.
    fn rev_list_before(
        &self,
        _dir: &Path,
        _rev: &str,
        _until_unix: i64,
    ) -> Result<Option<(String, i64)>, Error> {
        Err(Error::Git {
            action: "looking up the tap commit at or before the soak cutoff".into(),
            detail: "this git backend cannot walk history".into(),
        })
    }
    /// Force-set a pin ref to `sha`.
    fn update_ref(&self, _dir: &Path, ref_name: &str, _sha: &str) -> Result<(), Error> {
        Err(Error::Git {
            action: format!("pinning {ref_name}"),
            detail: "this git backend cannot update refs".into(),
        })
    }
    /// `git ls-tree -r --name-only <sha>`; cached per (dir, sha) in ProcessGit.
    fn ls_tree(&self, _dir: &Path, _sha: &str) -> Result<Vec<String>, Error> {
        Err(Error::Git {
            action: "listing a tap commit's files".into(),
            detail: "this git backend cannot list trees".into(),
        })
    }
}

type TreeCache = RefCell<HashMap<(PathBuf, String), Rc<Vec<String>>>>;

#[derive(Default)]
pub struct ProcessGit {
    /// `ls-tree` output per (clone dir, commit). A commit's tree never changes.
    trees: TreeCache,
}

/// Servers that do not support partial clone answer the filter with one of
/// these; the caller retries without it.
pub fn filter_rejected(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("filter")
        && (s.contains("not recognized")
            || s.contains("not support")
            || s.contains("invalid filter-spec"))
}

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

/// Git date options go through `approxidate`, which reads a bare integer as a
/// loose date (`0` means *now*, not the epoch) unless it is long enough to look
/// like a Unix timestamp. Spell the instant out so every value is unambiguous.
fn git_date_arg(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| unix.to_string())
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
        let since = format!("--shallow-since={}", git_date_arg(until_unix));
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
        let before = format!("--before={}", git_date_arg(until_unix));
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

    fn set_remote(&self, dir: &Path, name: &str, url: &str) -> Result<(), Error> {
        let action = format!("registering tap remote {name} ({url})");
        let dir = dir.to_string_lossy();
        let add = run_git(
            &action,
            &["--git-dir", dir.as_ref(), "remote", "add", name, url],
        )?;
        if add.status.success() {
            return Ok(());
        }
        if String::from_utf8_lossy(&add.stderr).contains("already exists") {
            let set = run_git(
                &action,
                &["--git-dir", dir.as_ref(), "remote", "set-url", name, url],
            )?;
            if set.status.success() {
                return Ok(());
            }
            return Err(git_fail(&action, &set));
        }
        Err(git_fail(&action, &add))
    }

    fn fetch_history(&self, dir: &Path, remote_name: &str, ref_name: &str) -> Result<(), Error> {
        let action = format!("fetching history from {remote_name} to find the tap's soak cutoff");
        let dir = dir.to_string_lossy();
        let spec = format!("+HEAD:{ref_name}");
        let with_filter = run_git(
            &action,
            &[
                "--git-dir",
                dir.as_ref(),
                "fetch",
                "--force",
                "--filter=blob:none",
                remote_name,
                &spec,
            ],
        )?;
        if with_filter.status.success() {
            return Ok(());
        }
        if !filter_rejected(&String::from_utf8_lossy(&with_filter.stderr)) {
            return Err(git_fail(&action, &with_filter));
        }
        let plain = run_git(
            &action,
            &[
                "--git-dir",
                dir.as_ref(),
                "fetch",
                "--force",
                remote_name,
                &spec,
            ],
        )?;
        if plain.status.success() {
            Ok(())
        } else {
            Err(git_fail(&action, &plain))
        }
    }

    fn rev_list_before(
        &self,
        dir: &Path,
        rev: &str,
        until_unix: i64,
    ) -> Result<Option<(String, i64)>, Error> {
        let action = "looking up the tap commit at or before the soak cutoff";
        let dir = dir.to_string_lossy();
        let before = format!("--before={}", git_date_arg(until_unix));
        // `log -1 --before` is the `rev-list -1 --before` walk with formatting
        // and no `commit <sha>` header line to strip.
        let output = run_git(
            action,
            &[
                "--git-dir",
                dir.as_ref(),
                "log",
                "-1",
                &before,
                "--format=%H %ct",
                rev,
            ],
        )?;
        if !output.status.success() {
            return if missing_object(&output) {
                Ok(None)
            } else {
                Err(git_fail(action, &output))
            };
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.split_whitespace();
        match (
            parts.next(),
            parts.next().and_then(|t| t.parse::<i64>().ok()),
        ) {
            (Some(sha), Some(when)) => Ok(Some((sha.to_string(), when))),
            _ => Ok(None),
        }
    }

    fn update_ref(&self, dir: &Path, ref_name: &str, sha: &str) -> Result<(), Error> {
        let action = format!("pinning {ref_name} to {sha}");
        let dir = dir.to_string_lossy();
        let output = run_git(
            &action,
            &["--git-dir", dir.as_ref(), "update-ref", ref_name, sha],
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_fail(&action, &output))
        }
    }

    fn ls_tree(&self, dir: &Path, sha: &str) -> Result<Vec<String>, Error> {
        let key = (dir.to_path_buf(), sha.to_string());
        if let Some(cached) = self.trees.borrow().get(&key) {
            return Ok(cached.as_ref().clone());
        }
        let action = format!("listing the files of tap commit {sha}");
        let dir_s = dir.to_string_lossy();
        let output = run_git(
            &action,
            &[
                "--git-dir",
                dir_s.as_ref(),
                "ls-tree",
                "-r",
                "--name-only",
                sha,
            ],
        )?;
        if !output.status.success() {
            return Err(git_fail(&action, &output));
        }
        let paths: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        self.trees.borrow_mut().insert(key, Rc::new(paths.clone()));
        Ok(paths)
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct InMemoryGit {
    blobs: RefCell<HashMap<(String, String), Vec<u8>>>,
    /// (dir, ref) -> sha. Tap clones each pin the same ref names.
    refs: RefCell<HashMap<(String, String), String>>,
    fetched: RefCell<Vec<(String, String)>>,
    /// remote url -> commits newest first (sha, committer unix)
    commits: RefCell<HashMap<String, Vec<(String, i64)>>>,
    trees: RefCell<HashMap<String, Vec<String>>>,
    remotes: RefCell<HashMap<(String, String), String>>, // (dir, name) -> url
    failing: RefCell<std::collections::HashSet<String>>,
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

    fn key(dir: &Path, name: &str) -> (String, String) {
        (dir.to_string_lossy().into_owned(), name.to_string())
    }

    pub fn insert_commits(&self, remote_url: &str, newest_first: &[(&str, i64)]) {
        self.commits.borrow_mut().insert(
            remote_url.to_string(),
            newest_first
                .iter()
                .map(|(s, t)| ((*s).to_string(), *t))
                .collect(),
        );
    }

    pub fn insert_tree(&self, sha: &str, paths: &[&str]) {
        self.trees.borrow_mut().insert(
            sha.to_string(),
            paths.iter().map(|p| (*p).to_string()).collect(),
        );
    }

    pub fn fail_remote(&self, remote_url: &str) {
        self.failing.borrow_mut().insert(remote_url.to_string());
    }

    pub fn remote_url(&self, dir: &Path) -> Option<String> {
        self.remotes
            .borrow()
            .get(&Self::key(dir, "origin"))
            .cloned()
    }
}

#[cfg(test)]
impl GitStore for InMemoryGit {
    fn init_bare(&self, _dir: &Path) -> Result<(), Error> {
        Ok(())
    }

    fn fetch_depth1(
        &self,
        dir: &Path,
        _remote: &str,
        sha: &str,
        ref_name: &str,
    ) -> Result<(), Error> {
        self.fetched
            .borrow_mut()
            .push((sha.to_string(), ref_name.to_string()));
        self.refs
            .borrow_mut()
            .insert(Self::key(dir, ref_name), sha.to_string());
        Ok(())
    }

    fn show(&self, _dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self
            .blobs
            .borrow()
            .get(&(sha.to_string(), path.to_string()))
            .cloned())
    }

    fn rev_parse(&self, dir: &Path, rev: &str) -> Result<Option<String>, Error> {
        Ok(self.refs.borrow().get(&Self::key(dir, rev)).cloned())
    }

    fn gc_prune(&self, _dir: &Path) -> Result<(), Error> {
        Ok(())
    }

    fn set_remote(&self, dir: &Path, name: &str, url: &str) -> Result<(), Error> {
        self.remotes
            .borrow_mut()
            .insert(Self::key(dir, name), url.to_string());
        Ok(())
    }

    fn fetch_history(&self, dir: &Path, remote_name: &str, ref_name: &str) -> Result<(), Error> {
        let url = self
            .remotes
            .borrow()
            .get(&Self::key(dir, remote_name))
            .cloned();
        let action = format!("fetching history from {remote_name} to find the tap's soak cutoff");
        let Some(url) = url else {
            return Err(Error::Git {
                action,
                detail: "no such remote".into(),
            });
        };
        if self.failing.borrow().contains(&url) {
            return Err(Error::Git {
                action,
                detail: format!("fatal: could not read from remote repository {url}"),
            });
        }
        let head = self
            .commits
            .borrow()
            .get(&url)
            .and_then(|c| c.first().map(|(s, _)| s.clone()));
        let Some(head) = head else {
            return Err(Error::Git {
                action,
                detail: "remote has no commits".into(),
            });
        };
        self.fetched
            .borrow_mut()
            .push((head.clone(), ref_name.to_string()));
        self.refs
            .borrow_mut()
            .insert(Self::key(dir, ref_name), head);
        Ok(())
    }

    fn rev_list_before(
        &self,
        dir: &Path,
        rev: &str,
        until_unix: i64,
    ) -> Result<Option<(String, i64)>, Error> {
        let start = self
            .refs
            .borrow()
            .get(&Self::key(dir, rev))
            .cloned()
            .unwrap_or_else(|| rev.to_string());
        let url = self
            .remotes
            .borrow()
            .get(&Self::key(dir, "origin"))
            .cloned();
        let commits = url
            .and_then(|u| self.commits.borrow().get(&u).cloned())
            .unwrap_or_default();
        let from = commits.iter().position(|(s, _)| *s == start).unwrap_or(0);
        Ok(commits[from..]
            .iter()
            .find(|(_, t)| *t <= until_unix)
            .cloned())
    }

    fn update_ref(&self, dir: &Path, ref_name: &str, sha: &str) -> Result<(), Error> {
        self.refs
            .borrow_mut()
            .insert(Self::key(dir, ref_name), sha.to_string());
        Ok(())
    }

    fn ls_tree(&self, _dir: &Path, sha: &str) -> Result<Vec<String>, Error> {
        Ok(self.trees.borrow().get(sha).cloned().unwrap_or_default())
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
    fn git_date_arg_spells_out_the_epoch() {
        // A bare `0` would reach git's approxidate as "now"; the shallow window
        // then selects nothing and the fetch fails.
        assert_eq!(git_date_arg(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn git_date_arg_spells_out_a_cutoff() {
        assert_eq!(git_date_arg(COMMIT_UNIX), "2023-11-14T22:13:20Z");
    }

    /// Commits with fixed author/committer dates so `--shallow-since` windows
    /// are not a race against the wall clock.
    const COMMIT_UNIX: i64 = 1_700_000_000;

    fn git_commit_at(dir: &Path, message: &str, unix: i64) {
        let date = format!("{unix} +0000");
        let out = Command::new("git")
            .current_dir(dir)
            .args(["commit", "-m", message])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("git commit");
        assert!(
            out.status.success(),
            "git commit -m {message}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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

        let git = ProcessGit::default();
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
        git_commit_at(&src, "one", COMMIT_UNIX);
        let older = git_ok(&src, &["rev-parse", "HEAD"]);
        std::fs::write(src.join("f"), "two\n").unwrap();
        git_ok(&src, &["add", "f"]);
        git_commit_at(&src, "two", COMMIT_UNIX + 60);
        let newer = git_ok(&src, &["rev-parse", "HEAD"]);

        let git = ProcessGit::default();
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
        let git = ProcessGit::default();
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
            "missing brewsoak framing: {text}"
        );
        assert!(
            text.to_lowercase()
                .contains("does not appear to be a git repository")
                || text.contains("fatal:"),
            "missing git output: {text}"
        );
    }

    #[test]
    fn in_memory_fetch_history_pins_head_and_rev_list_before_finds_cutoff() {
        let git = InMemoryGit::new();
        let dir = Path::new("/cache/taps/hashicorp/tap.git");
        git.insert_commits(
            "https://github.com/hashicorp/homebrew-tap",
            &[
                ("h3", 1_700_000_000),
                ("h2", 1_699_990_000),
                ("h1", 1_699_900_000),
            ],
        );
        git.set_remote(dir, "origin", "https://github.com/hashicorp/homebrew-tap")
            .unwrap();
        git.fetch_history(dir, "origin", REF_HEAD).unwrap();
        assert_eq!(git.rev_parse(dir, REF_HEAD).unwrap(), Some("h3".into()));
        assert_eq!(
            git.rev_list_before(dir, REF_HEAD, 1_699_995_000).unwrap(),
            Some(("h2".into(), 1_699_990_000))
        );
        assert_eq!(git.rev_list_before(dir, REF_HEAD, 1_000).unwrap(), None);
        git.update_ref(dir, REF_CUTOFF, "h2").unwrap();
        assert_eq!(git.rev_parse(dir, REF_CUTOFF).unwrap(), Some("h2".into()));
    }

    #[test]
    fn in_memory_refs_are_per_dir() {
        let git = InMemoryGit::new();
        let a = Path::new("/cache/taps/a/tap.git");
        let b = Path::new("/cache/taps/b/tap.git");
        git.update_ref(a, REF_HEAD, "aaa").unwrap();
        git.update_ref(b, REF_HEAD, "bbb").unwrap();
        assert_eq!(git.rev_parse(a, REF_HEAD).unwrap(), Some("aaa".into()));
        assert_eq!(git.rev_parse(b, REF_HEAD).unwrap(), Some("bbb".into()));
    }

    #[test]
    fn in_memory_failing_remote_is_error_git() {
        let git = InMemoryGit::new();
        let dir = Path::new("/cache/taps/x/y.git");
        git.fail_remote("https://example.com/x/homebrew-y");
        git.set_remote(dir, "origin", "https://example.com/x/homebrew-y")
            .unwrap();
        let err = git.fetch_history(dir, "origin", REF_HEAD).unwrap_err();
        assert!(matches!(err, Error::Git { .. }), "{err}");
        assert!(err.to_string().contains("fetching history"), "{err}");
    }

    #[test]
    fn in_memory_ls_tree_returns_inserted_paths() {
        let git = InMemoryGit::new();
        git.insert_tree("h2", &["Formula/terraform.rb", "README.md"]);
        assert_eq!(
            git.ls_tree(unused_dir(), "h2").unwrap(),
            vec!["Formula/terraform.rb".to_string(), "README.md".to_string()]
        );
        assert!(git.ls_tree(unused_dir(), "nosuch").unwrap().is_empty());
    }

    #[test]
    fn filter_rejected_recognizes_server_refusals() {
        assert!(filter_rejected(
            "fatal: filtering not recognized by server, ignoring"
        ));
        assert!(filter_rejected(
            "warning: filtering not recognized by server"
        ));
        assert!(filter_rejected("fatal: invalid filter-spec 'blob:none'"));
        assert!(filter_rejected(
            "fatal: the remote end hung up unexpectedly\nerror: server does not support filter"
        ));
        assert!(!filter_rejected(
            "fatal: could not read from remote repository"
        ));
    }

    #[test]
    fn process_git_fetch_history_then_cutoff_pin_and_ls_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bare = tmp.path().join("tap.git");
        std::fs::create_dir(&src).unwrap();
        git_ok(&src, &["init", "-b", "main"]);
        git_ok(&src, &["config", "user.email", "test@example.com"]);
        git_ok(&src, &["config", "user.name", "Test"]);
        std::fs::create_dir(src.join("Formula")).unwrap();
        std::fs::write(src.join("Formula/foo.rb"), "one\n").unwrap();
        git_ok(&src, &["add", "."]);
        git_commit_at(&src, "one", COMMIT_UNIX);
        let older = git_ok(&src, &["rev-parse", "HEAD"]);
        std::fs::write(src.join("Formula/foo.rb"), "two\n").unwrap();
        git_ok(&src, &["add", "."]);
        git_commit_at(&src, "two", COMMIT_UNIX + 3600);
        let newer = git_ok(&src, &["rev-parse", "HEAD"]);

        let git = ProcessGit::default();
        git.init_bare(&bare).expect("init");
        git.set_remote(&bare, "origin", src.to_str().unwrap())
            .expect("remote add");
        git.set_remote(&bare, "origin", src.to_str().unwrap())
            .expect("remote set-url is idempotent");
        git.fetch_history(&bare, "origin", REF_HEAD)
            .expect("fetch history");
        assert_eq!(git.rev_parse(&bare, REF_HEAD).unwrap(), Some(newer.clone()));
        let (sha, when) = git
            .rev_list_before(&bare, REF_HEAD, COMMIT_UNIX + 60)
            .unwrap()
            .expect("older commit is before the cutoff");
        assert_eq!(sha, older);
        assert_eq!(when, COMMIT_UNIX);
        assert_eq!(
            git.rev_list_before(&bare, REF_HEAD, COMMIT_UNIX - 1)
                .unwrap(),
            None
        );
        git.update_ref(&bare, REF_CUTOFF, &older)
            .expect("pin cutoff");
        assert_eq!(
            git.rev_parse(&bare, REF_CUTOFF).unwrap(),
            Some(older.clone())
        );
        assert_eq!(
            git.ls_tree(&bare, &older).unwrap(),
            vec!["Formula/foo.rb".to_string()]
        );
        assert_eq!(
            git.show(&bare, &older, "Formula/foo.rb")
                .unwrap()
                .as_deref(),
            Some(b"one\n".as_slice())
        );
        // Force-pin: moving HEAD back to the older commit must not be rejected.
        git_ok(&src, &["reset", "--hard", &older]);
        git.fetch_history(&bare, "origin", REF_HEAD)
            .expect("non-fast-forward head");
        assert_eq!(git.rev_parse(&bare, REF_HEAD).unwrap(), Some(older));
    }

    #[test]
    fn process_git_fetch_history_error_names_the_action() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("tap.git");
        let git = ProcessGit::default();
        git.init_bare(&bare).unwrap();
        git.set_remote(&bare, "origin", "/no/such/brewsoak-tap")
            .unwrap();
        let err = git.fetch_history(&bare, "origin", REF_HEAD).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("fetching history"), "{text}");
        assert!(text.contains("git failed"), "{text}");
    }
}
