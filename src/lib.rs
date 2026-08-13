pub mod brew;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod eligibility;
pub mod error;
pub mod git;
pub mod github;
pub mod hours;
pub mod identity;
pub mod paths;
pub mod resolve;
pub mod snapshot;
pub mod tap;

pub use error::Error;
pub use hours::SoakHours;

use crate::brew::Brew;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    Exit(i32),
    Exec(PathBuf, Vec<String>),
}

pub trait World {
    type Git: git::GitStore;
    type Github: github::GithubApi;
    type Brew: brew::Brew;

    fn config_path(&self) -> PathBuf;
    fn cache_path(&self) -> PathBuf;
    fn env_soak(&self) -> Option<String>;
    fn now(&self) -> time::OffsetDateTime;
    fn tap_root(&self) -> PathBuf;
    fn git(&self) -> &Self::Git;
    fn github(&self) -> &Self::Github;
    fn brew(&self) -> &Self::Brew;
}

pub struct RealWorld {
    git: git::ProcessGit,
    github: github::UreqGithub,
    brew: brew::ProcessBrew,
}

impl RealWorld {
    pub fn new() -> Self {
        Self {
            git: git::ProcessGit,
            github: github::UreqGithub {
                base: "https://api.github.com".into(),
            },
            brew: brew::ProcessBrew {
                bin: paths::brew_bin(),
            },
        }
    }
}

impl Default for RealWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl World for RealWorld {
    type Git = git::ProcessGit;
    type Github = github::UreqGithub;
    type Brew = brew::ProcessBrew;

    fn config_path(&self) -> PathBuf {
        paths::config_file()
    }

    fn cache_path(&self) -> PathBuf {
        paths::cache_dir()
    }

    fn env_soak(&self) -> Option<String> {
        std::env::var("BREWSOAK_SOAK_HOURS").ok()
    }

    fn now(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }

    fn tap_root(&self) -> PathBuf {
        real_tap_root(self.brew())
    }

    fn git(&self) -> &git::ProcessGit {
        &self.git
    }

    fn github(&self) -> &github::UreqGithub {
        &self.github
    }

    fn brew(&self) -> &brew::ProcessBrew {
        &self.brew
    }
}

fn real_tap_root(brew: &impl brew::Brew) -> PathBuf {
    const TAP_REL: &str = "Library/Taps/brewsoakr/homebrew-soaked";
    match brew.run(&["--repository".into()]) {
        Ok(output) if output.status.success() => {
            let repo = String::from_utf8_lossy(&output.stdout);
            let repo = repo.trim();
            if repo.is_empty() {
                PathBuf::from("/opt/homebrew").join(TAP_REL)
            } else {
                PathBuf::from(repo).join(TAP_REL)
            }
        }
        _ => PathBuf::from("/opt/homebrew").join(TAP_REL),
    }
}

pub fn run(args: &[String]) -> i32 {
    match dispatch(args, &RealWorld::new()) {
        Ok(Dispatch::Exit(c)) => c,
        Ok(Dispatch::Exec(bin, argv)) => brew::exec(&bin, &argv),
        Err(e) => {
            eprintln!("brewsoakr: {e}");
            e.exit_code()
        }
    }
}

pub fn dispatch(args: &[String], world: &impl World) -> Result<Dispatch, Error> {
    let inv = cli::parse_argv(args)?;
    let env = world.env_soak();
    let file = config::read_file(&world.config_path());
    let resolved = config::resolve_hours(inv.soak_hours, env.as_deref(), file.as_deref())?;
    config::apply_persist(resolved.persist, &world.config_path())?;

    match inv.command {
        cli::Command::Passthrough { args } => {
            Ok(Dispatch::Exec(world.brew().brew_bin().to_path_buf(), args))
        }
        cli::Command::Update => {
            let cache = world.cache_path();
            let mut out = std::io::stdout();
            cmd::update(
                world.git(),
                world.github(),
                &cache,
                resolved.hours,
                world.now(),
                &mut out,
            )?;
            if let Ok(Some(snaps)) = snapshot::load_state(&cache) {
                cmd::prefetch_installed(world.brew(), world.git(), &cache, &snaps);
            }
            Ok(Dispatch::Exit(0))
        }
        cli::Command::Outdated => {
            let cache = world.cache_path();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                world.brew(),
                &cache,
                resolved.hours,
                world.now(),
                false,
            )?;
            let mut out = std::io::stdout();
            soaked_exit(cmd::outdated(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &mut out,
            ))
        }
        cli::Command::Info { names } => {
            let cache = world.cache_path();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                world.brew(),
                &cache,
                resolved.hours,
                world.now(),
                false,
            )?;
            let mut out = std::io::stdout();
            soaked_exit(cmd::info(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &names,
                &mut out,
            ))
        }
        cli::Command::Upgrade { names } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                world.brew(),
                &cache,
                resolved.hours,
                world.now(),
                true,
            )?;
            let mut out = std::io::stdout();
            soaked_exit(cmd::upgrade(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &tap_root,
                &names,
                &inv.brew_args,
                &mut out,
            ))
        }
        cli::Command::Install {
            names,
            force_cask,
            force_formula,
        } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                world.brew(),
                &cache,
                resolved.hours,
                world.now(),
                true,
            )?;
            let mut out = std::io::stdout();
            soaked_exit(cmd::install(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &tap_root,
                &names,
                force_cask,
                force_formula,
                &inv.brew_args,
                &mut out,
            ))
        }
        cli::Command::Reinstall { names } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                world.brew(),
                &cache,
                resolved.hours,
                world.now(),
                true,
            )?;
            let mut out = std::io::stdout();
            soaked_exit(cmd::reinstall(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &tap_root,
                &names,
                &inv.brew_args,
                &mut out,
            ))
        }
    }
}

fn soaked_exit(result: Result<cmd::RunResult, Error>) -> Result<Dispatch, Error> {
    match result {
        Ok(r) => Ok(Dispatch::Exit(cmd::combine_exit(r.refused, r.brew_status))),
        Err(Error::Brew { status, message }) => {
            if !message.is_empty() {
                eprintln!("brewsoakr: {message}");
            }
            Ok(Dispatch::Exit(cmd::combine_exit(false, Some(status))))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::MockBrew;
    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};
    use std::cell::Cell;
    use std::path::PathBuf;
    use time::{Duration, OffsetDateTime};

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

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

    struct TestWorld {
        config_path: PathBuf,
        cache_path: PathBuf,
        tap_root: PathBuf,
        env_soak: Option<String>,
        git: InMemoryGit,
        github: RecordingGithub,
        brew: MockBrew,
        _tmp: tempfile::TempDir,
    }

    struct RecordingGithub {
        inner: StaticGithub,
        refreshed: Cell<bool>,
    }

    impl github::GithubApi for RecordingGithub {
        fn head_sha(&self, repo: &str) -> Result<String, Error> {
            self.refreshed.set(true);
            self.inner.head_sha(repo)
        }

        fn latest_commit_until(
            &self,
            repo: &str,
            until: OffsetDateTime,
        ) -> Result<CommitInfo, Error> {
            self.refreshed.set(true);
            self.inner.latest_commit_until(repo, until)
        }
    }

    impl TestWorld {
        fn new() -> Self {
            Self::with_github(fixture_gh())
        }

        fn with_github(inner: StaticGithub) -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let config_path = tmp.path().join(".config/brewsoak/config.toml");
            let cache_path = tmp.path().join("Library/Caches/brewsoak");
            let tap_root = tmp.path().join("tap");
            Self {
                config_path,
                cache_path,
                tap_root,
                env_soak: None,
                git: InMemoryGit::new(),
                github: RecordingGithub {
                    inner,
                    refreshed: Cell::new(false),
                },
                brew: MockBrew::new(),
                _tmp: tmp,
            }
        }
    }

    impl World for TestWorld {
        type Git = InMemoryGit;
        type Github = RecordingGithub;
        type Brew = MockBrew;

        fn config_path(&self) -> PathBuf {
            self.config_path.clone()
        }
        fn cache_path(&self) -> PathBuf {
            self.cache_path.clone()
        }
        fn env_soak(&self) -> Option<String> {
            self.env_soak.clone()
        }
        fn now(&self) -> OffsetDateTime {
            now()
        }
        fn tap_root(&self) -> PathBuf {
            self.tap_root.clone()
        }
        fn git(&self) -> &InMemoryGit {
            &self.git
        }
        fn github(&self) -> &RecordingGithub {
            &self.github
        }
        fn brew(&self) -> &MockBrew {
            &self.brew
        }
    }

    #[test]
    fn passthrough_services_is_exec_without_soak_flag() {
        let world = TestWorld::new();
        match dispatch(&s(&["services", "start", "foo"]), &world).expect("dispatch") {
            Dispatch::Exec(_bin, argv) => {
                assert_eq!(argv, s(&["services", "start", "foo"]));
                assert!(!argv.iter().any(|a| a.contains("soak-hours")));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn soak_hours_persists_and_strips_from_passthrough() {
        let world = TestWorld::new();
        match dispatch(
            &s(&["--soak-hours", "48", "services", "start", "x"]),
            &world,
        )
        .expect("dispatch")
        {
            Dispatch::Exec(_bin, argv) => {
                assert_eq!(argv, s(&["services", "start", "x"]));
                assert!(!argv.iter().any(|a| a.contains("soak-hours")));
            }
            other => panic!("{other:?}"),
        }
        let text = std::fs::read_to_string(world.config_path()).expect("persisted config");
        assert_eq!(text, "SOAK_HOURS = 48\n");
    }

    #[test]
    fn outdated_empty_snapshots_refreshes() {
        let world = TestWorld::new();
        assert!(!world.cache_path.join("state.toml").exists());
        match dispatch(&s(&["outdated"]), &world).expect("dispatch") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            world.github.refreshed.get(),
            "refresh should have queried github"
        );
    }
}
