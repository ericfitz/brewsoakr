use crate::git::{GitStore, REF_CUTOFF, REF_HEAD};
use crate::github::{GithubApi, cutoff_instant};
use crate::{Error, SoakHours};
use std::path::Path;
use std::process::{Command, Stdio};
use time::OffsetDateTime;

pub const CORE_REMOTE: &str = "https://github.com/Homebrew/homebrew-core";
pub const CASK_REMOTE: &str = "https://github.com/Homebrew/homebrew-cask";
pub const CORE_REPO: &str = "Homebrew/homebrew-core";
pub const CASK_REPO: &str = "Homebrew/homebrew-cask";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapSnapshot {
    pub cutoff_sha: String,
    pub head_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshots {
    pub core: TapSnapshot,
    pub cask: TapSnapshot,
    pub hours: SoakHours,
}

#[derive(serde::Deserialize)]
struct StateFile {
    hours: u32,
    core_cutoff: String,
    core_head: String,
    cask_cutoff: String,
    cask_head: String,
}

pub fn refresh(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: time::OffsetDateTime,
) -> Result<Snapshots, Error> {
    std::fs::create_dir_all(cache)?;
    let until = cutoff_instant(now, hours);
    let core = refresh_tap(
        git,
        gh,
        &cache.join("core.git"),
        CORE_REMOTE,
        CORE_REPO,
        until,
    )?;
    let cask = refresh_tap(
        git,
        gh,
        &cache.join("cask.git"),
        CASK_REMOTE,
        CASK_REPO,
        until,
    )?;
    let snaps = Snapshots { core, cask, hours };
    write_state(cache, &snaps)?;
    Ok(snaps)
}

pub fn load_state(cache: &Path) -> Result<Option<Snapshots>, Error> {
    let path = cache.join("state.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let parsed: StateFile =
        toml::from_str(&raw).map_err(|e| Error::Other(format!("state.toml: {e}")))?;
    let hours = SoakHours::new(parsed.hours)
        .ok_or_else(|| Error::Other(format!("invalid hours in state.toml: {}", parsed.hours)))?;
    Ok(Some(Snapshots {
        core: TapSnapshot {
            cutoff_sha: parsed.core_cutoff,
            head_sha: parsed.core_head,
        },
        cask: TapSnapshot {
            cutoff_sha: parsed.cask_cutoff,
            head_sha: parsed.cask_head,
        },
        hours,
    }))
}

fn refresh_tap(
    git: &impl GitStore,
    gh: &impl GithubApi,
    dir: &Path,
    remote: &str,
    repo: &str,
    until: OffsetDateTime,
) -> Result<TapSnapshot, Error> {
    git.init_bare(dir)?;
    let head_sha = gh.head_sha(repo)?;
    let cutoff_sha = match gh.latest_commit_until(repo, until) {
        Ok(info) => info.sha,
        Err(_) => cutoff_via_shallow(git, dir, remote, until)?,
    };
    git.fetch_depth1(dir, remote, &cutoff_sha, REF_CUTOFF)?;
    git.fetch_depth1(dir, remote, &head_sha, REF_HEAD)?;
    git.gc_prune(dir)?;
    Ok(TapSnapshot {
        cutoff_sha,
        head_sha,
    })
}

fn write_state(cache: &Path, snaps: &Snapshots) -> Result<(), Error> {
    let body = format!(
        "hours = {}\ncore_cutoff = \"{}\"\ncore_head = \"{}\"\ncask_cutoff = \"{}\"\ncask_head = \"{}\"\n",
        snaps.hours.get(),
        snaps.core.cutoff_sha,
        snaps.core.head_sha,
        snaps.cask.cutoff_sha,
        snaps.cask.head_sha,
    );
    std::fs::write(cache.join("state.toml"), body)?;
    Ok(())
}

/// Resolve cutoff via `git fetch --shallow-since` when GitHub lookup fails.
/// Only ProcessGit creates a bare `HEAD`; InMemoryGit cannot shallow-fetch.
fn cutoff_via_shallow(
    _git: &impl GitStore,
    dir: &Path,
    remote: &str,
    until: OffsetDateTime,
) -> Result<String, Error> {
    if !dir.join("HEAD").exists() {
        return Err(Error::Other("github and git fallback failed".into()));
    }
    let unix = until.unix_timestamp();
    let fetch = Command::new("git")
        .stdin(Stdio::null())
        .arg("--git-dir")
        .arg(dir)
        .arg("fetch")
        .arg(format!("--shallow-since={unix}"))
        .arg(remote)
        .arg("HEAD")
        .output();
    if !matches!(fetch, Ok(ref o) if o.status.success()) {
        return Err(Error::Other("github and git fallback failed".into()));
    }
    let log = Command::new("git")
        .stdin(Stdio::null())
        .arg("--git-dir")
        .arg(dir)
        .arg("log")
        .arg("-1")
        .arg(format!("--before={unix}"))
        .arg("--format=%H")
        .output();
    match log {
        Ok(o) if o.status.success() => {
            let sha = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if sha.is_empty() {
                Err(Error::Other("github and git fallback failed".into()))
            } else {
                Ok(sha)
            }
        }
        _ => Err(Error::Other("github and git fallback failed".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("fixed now")
    }

    fn fixture_gh() -> StaticGithub {
        let now = now();
        StaticGithub {
            head: "headsha".into(),
            commits: vec![
                CommitInfo {
                    sha: "headsha".into(),
                    committer_time: now - Duration::hours(2),
                },
                CommitInfo {
                    sha: "tenh".into(),
                    committer_time: now - Duration::hours(10),
                },
                CommitInfo {
                    sha: "thirtyh".into(),
                    committer_time: now - Duration::hours(30),
                },
            ],
        }
    }

    #[test]
    fn refresh_sets_core_cutoff_to_pre_soak_commit() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let hours = SoakHours::new(24).expect("hours >= 1");
        let snaps = refresh(&git, &fixture_gh(), dir.path(), hours, now()).expect("refresh");
        assert_eq!(snaps.core.cutoff_sha, "thirtyh");
        assert_eq!(snaps.core.head_sha, "headsha");
        assert_eq!(snaps.cask.cutoff_sha, "thirtyh");
        assert_eq!(snaps.cask.head_sha, "headsha");
        assert_eq!(snaps.hours, hours);
    }

    #[test]
    fn load_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_state(dir.path()).expect("missing state").is_none());

        let git = InMemoryGit::new();
        let hours = SoakHours::new(24).expect("hours >= 1");
        let snaps = refresh(&git, &fixture_gh(), dir.path(), hours, now()).expect("refresh");
        let loaded = load_state(dir.path())
            .expect("load")
            .expect("state.toml written");
        assert_eq!(loaded.core.cutoff_sha, snaps.core.cutoff_sha);
        assert_eq!(loaded.core.head_sha, snaps.core.head_sha);
        assert_eq!(loaded.cask.cutoff_sha, snaps.cask.cutoff_sha);
        assert_eq!(loaded.cask.head_sha, snaps.cask.head_sha);
        assert_eq!(loaded.hours, snaps.hours);

        let raw = std::fs::read_to_string(dir.path().join("state.toml")).expect("read state");
        assert!(raw.contains("hours = 24"), "{raw}");
        assert!(!raw.contains("SOAK_HOURS"), "{raw}");
    }

    #[test]
    fn smaller_hours_moves_cutoff_forward() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let gh = fixture_gh();

        let first = refresh(
            &git,
            &gh,
            dir.path(),
            SoakHours::new(24).expect("24"),
            now(),
        )
        .expect("refresh 24h");
        assert_eq!(first.core.cutoff_sha, "thirtyh");

        let second = refresh(&git, &gh, dir.path(), SoakHours::new(8).expect("8"), now())
            .expect("refresh 8h");
        assert_eq!(second.core.cutoff_sha, "tenh");
        assert_eq!(second.core.head_sha, "headsha");
        assert_eq!(second.hours.get(), 8);

        let loaded = load_state(dir.path())
            .expect("load")
            .expect("state.toml written");
        assert_eq!(loaded.core.cutoff_sha, "tenh");
        assert_eq!(loaded.hours.get(), 8);
    }
}
