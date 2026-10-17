pub mod atomic;
pub mod brew;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod eligibility;
pub mod error;
pub mod flags;
pub mod git;
pub mod github;
pub mod hours;
pub mod identity;
pub mod inventory;
pub mod lock;
pub mod nosoak;
pub mod origin;
pub mod paths;
pub mod prune;
pub mod quiet;
pub mod report;
pub mod resolve;
pub mod settings;
pub mod snapshot;
pub mod tap;
pub mod taps;

pub use error::Error;
pub use hours::SoakHours;

use crate::brew::Brew;
use std::io::Write;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use toml_edit::DocumentMut;

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
    /// How long a run waits for another run's cache lock.
    fn lock_wait(&self) -> std::time::Duration {
        lock::WAIT
    }
}

pub struct RealWorld {
    git: git::ProcessGit,
    github: github::UreqGithub,
    brew: brew::ProcessBrew,
}

impl RealWorld {
    pub fn new() -> Self {
        Self {
            git: git::ProcessGit::default(),
            github: github::UreqGithub {
                base: "https://api.github.com".into(),
            },
            brew: brew::ProcessBrew::new(paths::brew_bin()),
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
        // Must live outside Homebrew's Taps/ so `brew install /path/foo.rb`
        // is not treated as brewsoak/soaked (same-name tap conflict).
        self.cache_path().join("staging")
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

pub fn run(args: &[String]) -> i32 {
    match dispatch(args, &RealWorld::new(), &mut std::io::stdout()) {
        Ok(Dispatch::Exit(c)) => c,
        Ok(Dispatch::Exec(bin, argv)) => brew::exec(&bin, &argv),
        Err(e) => {
            eprintln!("brewsoak: {e}");
            e.exit_code()
        }
    }
}

/// Parse and run one command line. Everything brewsoak prints on stdout, and
/// the live output of visible `brew` runs, goes through `out` (production
/// passes stdout), so the two stay in order. Warnings go to stderr.
pub fn dispatch(
    args: &[String],
    world: &impl World,
    out: &mut impl Write,
) -> Result<Dispatch, Error> {
    paths::warn_duplicate_install();
    let inv = cli::parse_argv(args)?;
    world.brew().set_raw(inv.raw);
    world
        .brew()
        .set_cli_languages(flags::language_list(&inv.brew_args));
    if let cli::Command::Settings(cmd) = &inv.command {
        // Before config resolution, brew, git, or snapshots: settings reads
        // and writes the file and nothing else, and refuses invalid TOML
        // where resolve_config would silently default it.
        let env = world.env_soak();
        run_settings(
            cmd,
            &world.config_path(),
            env.as_deref(),
            world.now(),
            &mut *out,
        )?;
        return Ok(Dispatch::Exit(0));
    }
    // Everything below the settings branch that writes the cache holds the
    // run lock to the end of dispatch. `update`, `upgrade`, `install` and
    // `reinstall` take it up front. `outdated` and `info` write the cache only
    // when the stored snapshot is missing or a tap is stale, so
    // `cmd::ensure_snapshots` takes it from this slot at that moment, after
    // which it re-reads the snapshot and decides again. Version, help and
    // passthrough never touch the cache.
    let mut lock = lock::LockSlot::new(&world.cache_path(), args, world.lock_wait());
    match inv.command {
        cli::Command::Update
        | cli::Command::Upgrade { .. }
        | cli::Command::Install { .. }
        | cli::Command::Reinstall { .. } => {
            lock.get()?;
        }
        cli::Command::Outdated
        | cli::Command::Info { .. }
        | cli::Command::Version
        | cli::Command::Help { .. }
        | cli::Command::Settings(_)
        | cli::Command::Passthrough { .. } => {}
    }
    let env = world.env_soak();
    let file = config::read_file(&world.config_path());
    let cfg = config::resolve_config(inv.soak_hours, env.as_deref(), file.as_deref())?;
    if !matches!(
        inv.command,
        cli::Command::Version | cli::Command::Help { .. }
    ) {
        for w in &cfg.warnings {
            eprintln!("brewsoak: warning: {w}");
        }
    }
    if inv.command.is_soaked() {
        // A dry run changes nothing on the machine, the config file included.
        if !flags::is_dry_run(&inv.brew_args)
            && let Some(warning) =
                config::apply_persist(cfg.persist, &world.config_path(), world.now())?
        {
            eprintln!("brewsoak: warning: {warning}");
        }
        if cmd::is_verbose(&inv.brew_args) {
            for note in &cfg.notes {
                writeln!(out, "{note}")?;
            }
        }
    }

    match inv.command {
        cli::Command::Version => {
            writeln!(out, "{}", cli::version_line())?;
            Ok(Dispatch::Exit(0))
        }
        cli::Command::Help { topic: None } => {
            write!(out, "{}", cli::help_text())?;
            Ok(Dispatch::Exit(0))
        }
        cli::Command::Help { topic: Some(topic) } => {
            if let Some(text) = cli::command_help(&topic) {
                write!(out, "{text}")?;
                Ok(Dispatch::Exit(0))
            } else {
                Ok(Dispatch::Exec(
                    world.brew().brew_bin().to_path_buf(),
                    vec!["help".into(), topic],
                ))
            }
        }
        cli::Command::Settings(_) => unreachable!("settings returns before config resolution"),
        cli::Command::Passthrough { args } => {
            Ok(Dispatch::Exec(world.brew().brew_bin().to_path_buf(), args))
        }
        cli::Command::Update => {
            let cache = world.cache_path();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            soaked_exit(
                cmd::update(
                    lock.get()?,
                    world.brew(),
                    world.git(),
                    world.github(),
                    &cache,
                    &cfg,
                    &pkgs,
                    world.now(),
                    cmd::is_verbose(&inv.brew_args),
                    &mut *out,
                )
                .map(|()| cmd::RunResult {
                    refused: false,
                    brew_status: None,
                }),
            )
        }
        cli::Command::Outdated => {
            let cache = world.cache_path();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let snaps = cmd::ensure_snapshots(
                &mut lock,
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &[],
                world.now(),
                false,
                cmd::is_verbose(&inv.brew_args),
                &mut *out,
            )?;
            soaked_exit(cmd::outdated(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &pkgs,
                &cfg,
                &inv.brew_args,
                &mut *out,
            ))
        }
        cli::Command::Info { names } => {
            let cache = world.cache_path();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let extra = cmd::explicit_tap_tokens(&names);
            let snaps = cmd::ensure_snapshots(
                &mut lock,
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &extra,
                world.now(),
                false,
                cmd::is_verbose(&inv.brew_args),
                &mut *out,
            )?;
            soaked_exit(cmd::info(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &pkgs,
                &cfg,
                &names,
                &inv.brew_args,
                &mut *out,
            ))
        }
        cli::Command::Upgrade { names } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let snaps = cmd::ensure_snapshots(
                &mut lock,
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &cmd::explicit_tap_tokens(&names),
                world.now(),
                true,
                cmd::is_verbose(&inv.brew_args),
                &mut *out,
            )?;
            soaked_exit(cmd::upgrade(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &tap_root,
                &pkgs,
                &cfg,
                &names,
                &inv.brew_args,
                &mut *out,
            ))
        }
        cli::Command::Install {
            names,
            force_cask,
            force_formula,
        } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let snaps = cmd::ensure_snapshots(
                &mut lock,
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &cmd::explicit_tap_tokens(&names),
                world.now(),
                true,
                cmd::is_verbose(&inv.brew_args),
                &mut *out,
            )?;
            soaked_exit(cmd::install(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &tap_root,
                &pkgs,
                &cfg,
                &names,
                force_cask,
                force_formula,
                &inv.brew_args,
                &mut *out,
            ))
        }
        cli::Command::Reinstall { names } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let snaps = cmd::ensure_snapshots(
                &mut lock,
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &cmd::explicit_tap_tokens(&names),
                world.now(),
                true,
                cmd::is_verbose(&inv.brew_args),
                &mut *out,
            )?;
            soaked_exit(cmd::reinstall(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &tap_root,
                &pkgs,
                &cfg,
                &names,
                &inv.brew_args,
                &mut *out,
            ))
        }
    }
}

/// Spec "brewsoak settings". Reads the file once, applies one edit, and
/// writes the whole document back only when the edit changed it.
pub fn run_settings(
    cmd: &cli::SettingsCmd,
    path: &Path,
    env: Option<&str>,
    now: OffsetDateTime,
    out: &mut impl Write,
) -> Result<(), Error> {
    let existing = config::read_existing(path)?;
    let doc = match existing.as_deref() {
        None => None,
        Some(text) => Some(text.parse::<DocumentMut>().map_err(|e| {
            Error::Refusal(format!(
                "{} is not valid TOML; fix it by hand before using settings:\n{e}",
                path.display()
            ))
        })?),
    };
    match cmd {
        cli::SettingsCmd::Show => {
            write!(out, "{}", settings::render_show(path, env, doc.as_ref()))?;
            Ok(())
        }
        cli::SettingsCmd::Edit(edit) => {
            edit_settings(edit, doc.unwrap_or_default(), path, env, now, out)
        }
    }
}

fn edit_settings(
    cmd: &cli::SettingsEdit,
    mut doc: DocumentMut,
    path: &Path,
    env: Option<&str>,
    now: OffsetDateTime,
    out: &mut impl Write,
) -> Result<(), Error> {
    // The reader's warnings (a NO_SOAK inside [[TAP]]) fire on edits too;
    // dispatch skipped its own warning loop for settings.
    // Repair reports what it fixes, warnings included.
    if !matches!(cmd, cli::SettingsEdit::NoSoakRepair) {
        for warning in &config::parse_file(&doc.to_string()).warnings {
            eprintln!("brewsoak: warning: {warning}");
        }
    }
    let edit = match cmd {
        cli::SettingsEdit::SoakHours(hours) => {
            if let Some(raw) = env {
                eprintln!(
                    "brewsoak: warning: BREWSOAK_SOAK_HOURS={raw} is set and overrides SOAK_HOURS in the file"
                );
            }
            match hours {
                Some(h) => settings::set_soak_hours(&mut doc, *h),
                None => settings::clear_soak_hours(&mut doc),
            }
        }
        cli::SettingsEdit::NoSoakAdd(tokens) => settings::no_soak_add(&mut doc, tokens)?,
        cli::SettingsEdit::NoSoakRemove(tokens) => settings::no_soak_remove(&mut doc, tokens)?,
        cli::SettingsEdit::NoSoakRepair => settings::no_soak_repair(&mut doc)?,
        cli::SettingsEdit::TapHours {
            tap,
            hours: Some(h),
        } => settings::tap_hours_set(&mut doc, tap, *h)?,
        cli::SettingsEdit::TapHours { tap, hours: None } => {
            settings::tap_hours_clear(&mut doc, tap)?
        }
    };
    for message in &edit.messages {
        writeln!(out, "{message}")?;
    }
    if !edit.changed {
        return Ok(());
    }
    let body = doc.to_string();
    config::write_atomic(path, &body, now)?;
    if body.trim().is_empty() {
        writeln!(out, "removed {} (nothing left)", path.display())?;
    } else {
        writeln!(out, "wrote {}", path.display())?;
    }
    Ok(())
}

fn soaked_exit(result: Result<cmd::RunResult, Error>) -> Result<Dispatch, Error> {
    match result {
        Ok(r) => Ok(Dispatch::Exit(cmd::combine_exit(r.refused, r.brew_status))),
        Err(Error::Brew { status, message }) => {
            if !message.is_empty() {
                eprintln!("brewsoak: {message}");
            }
            Ok(Dispatch::Exit(cmd::combine_exit(false, Some(status))))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::{InstalledPkg, MockBrew};
    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};
    use crate::resolve::PkgKind;
    use crate::taps::TapInfo;
    use std::cell::Cell;
    use std::path::PathBuf;
    use time::{Duration, OffsetDateTime};

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("fixed now")
    }

    /// `dispatch` with its stdout captured in a buffer, never the real stdout.
    fn dispatch_captured(
        args: &[String],
        world: &impl World,
    ) -> (Result<Dispatch, Error>, Vec<u8>) {
        let mut out = Vec::new();
        let result = dispatch(args, world, &mut out);
        (result, out)
    }

    fn dispatch_quiet(args: &[String], world: &impl World) -> Result<Dispatch, Error> {
        dispatch_captured(args, world).0
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
                CommitInfo {
                    sha: "sixtyh".into(),
                    committer_time: now - Duration::hours(60),
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
        lock_wait: std::time::Duration,
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
                lock_wait: std::time::Duration::from_millis(150),
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
        fn lock_wait(&self) -> std::time::Duration {
            self.lock_wait
        }
    }

    /// A mock brew whose visible runs really spawn `/bin/echo`, so a dispatch
    /// run produces both brewsoak lines and live brew output.
    struct EchoVisibleBrew {
        mock: MockBrew,
        real: brew::ProcessBrew,
    }

    impl Brew for EchoVisibleBrew {
        fn brew_bin(&self) -> &Path {
            self.mock.brew_bin()
        }
        fn run(&self, a: &[String]) -> Result<std::process::Output, Error> {
            self.mock.run(a)
        }
        fn run_visible(
            &self,
            a: &[String],
            terminal: &mut dyn Write,
        ) -> Result<std::process::Output, Error> {
            self.real.run_visible(a, terminal)
        }
        fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
            self.mock.installed_packages()
        }
        fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
            self.mock.tap_info()
        }
        fn outdated_names(&self, greedy: flags::Greedy) -> Result<Vec<String>, Error> {
            self.mock.outdated_names(greedy)
        }
        fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
            self.mock.deps(kind, token)
        }
    }

    struct EchoWorld {
        inner: TestWorld,
        brew: EchoVisibleBrew,
    }

    impl World for EchoWorld {
        type Git = InMemoryGit;
        type Github = RecordingGithub;
        type Brew = EchoVisibleBrew;

        fn config_path(&self) -> PathBuf {
            self.inner.config_path()
        }
        fn cache_path(&self) -> PathBuf {
            self.inner.cache_path()
        }
        fn env_soak(&self) -> Option<String> {
            None
        }
        fn now(&self) -> OffsetDateTime {
            now()
        }
        fn tap_root(&self) -> PathBuf {
            self.inner.tap_root()
        }
        fn git(&self) -> &InMemoryGit {
            &self.inner.git
        }
        fn github(&self) -> &RecordingGithub {
            &self.inner.github
        }
        fn brew(&self) -> &EchoVisibleBrew {
            &self.brew
        }
        fn lock_wait(&self) -> std::time::Duration {
            self.inner.lock_wait
        }
    }

    #[test]
    fn dispatch_interleaves_brewsoak_and_brew_lines_in_the_injected_writer() {
        let inner = TestWorld::new();
        std::fs::create_dir_all(inner.config_path.parent().unwrap()).unwrap();
        std::fs::write(&inner.config_path, "NO_SOAK = [\"wget\"]\n").unwrap();
        let world = EchoWorld {
            inner,
            brew: EchoVisibleBrew {
                mock: MockBrew {
                    installed: vec![InstalledPkg {
                        name: "wget".into(),
                        kind: PkgKind::Formula,
                        receipt_rb: "class Wget < Formula\nend\n".into(),
                        pinned: false,
                        tap: None,
                        staged_path: None,
                    }],
                    ..MockBrew::new()
                },
                real: brew::ProcessBrew::new(PathBuf::from("/bin/echo")),
            },
        };
        let (result, out) = dispatch_captured(&s(&["update"]), &world);
        assert!(matches!(result, Ok(Dispatch::Exit(0))), "{result:?}");
        let text = String::from_utf8(out).unwrap();
        let before = text.find("no-soak packages installed; updating brew\n");
        let brew_line = text.find("\nupdate\n");
        let after = text.find("snapshots refreshed\n");
        assert!(
            matches!((before, brew_line, after), (Some(a), Some(b), Some(c)) if a < b && b < c),
            "brewsoak and brew lines must interleave in the injected writer: {text:?}"
        );
    }

    #[test]
    fn cache_writing_commands_fail_while_another_run_holds_the_lock() {
        for argv in [
            s(&["update"]),
            s(&["outdated"]),
            s(&["info", "wget"]),
            s(&["upgrade"]),
            s(&["install", "wget"]),
            s(&["reinstall", "wget"]),
        ] {
            let world = TestWorld::new();
            let _held = lock::acquire_waiting(
                &world.cache_path,
                &s(&["upgrade", "jq"]),
                std::time::Duration::from_millis(50),
            )
            .unwrap();
            let err = dispatch_quiet(&argv, &world).unwrap_err();
            let text = err.to_string();
            assert!(matches!(err, Error::Refusal(_)), "{argv:?}: {text}");
            assert!(
                text.contains(&format!("pid {}", std::process::id()))
                    && text.contains("`brewsoak upgrade jq`"),
                "{argv:?}: {text}"
            );
            assert!(!world.github.refreshed.get(), "{argv:?} did work");
            assert!(!world.config_path.exists(), "{argv:?} wrote config");
        }
    }

    #[test]
    fn outdated_and_info_with_fresh_snapshots_read_without_the_lock() {
        let world = TestWorld::new();
        // The first run has no stored snapshot, so it refreshes under the lock.
        dispatch_quiet(&s(&["outdated"]), &world).unwrap();
        for argv in [s(&["outdated"]), s(&["info", "wget"])] {
            world.github.refreshed.set(false);
            let _held = lock::acquire_waiting(
                &world.cache_path,
                &s(&["upgrade", "jq"]),
                std::time::Duration::from_millis(50),
            )
            .unwrap();
            dispatch_quiet(&argv, &world).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(!world.github.refreshed.get(), "{argv:?} refreshed");
        }
    }

    #[test]
    fn outdated_and_info_that_must_refresh_still_need_the_lock() {
        for argv in [s(&["outdated"]), s(&["info", "wget"])] {
            let world = TestWorld::new();
            dispatch_quiet(&s(&["outdated"]), &world).unwrap();
            // A corrupt stored snapshot forces the refresh path.
            std::fs::write(world.cache_path.join("state.toml"), "[[[").unwrap();
            world.github.refreshed.set(false);
            let _held = lock::acquire_waiting(
                &world.cache_path,
                &s(&["upgrade", "jq"]),
                std::time::Duration::from_millis(50),
            )
            .unwrap();
            let err = dispatch_quiet(&argv, &world).unwrap_err();
            assert!(matches!(err, Error::Refusal(_)), "{argv:?}: {err}");
            assert!(!world.github.refreshed.get(), "{argv:?} did work");
            let state = std::fs::read_to_string(world.cache_path.join("state.toml")).unwrap();
            assert_eq!(state, "[[[", "{argv:?} wrote state.toml without the lock");
        }
    }

    #[test]
    fn a_waiter_decides_again_after_it_gets_the_lock() {
        // The run that held the lock refreshed while this one waited, so the
        // waiter finds fresh state and must not fetch or write anything.
        for argv in [s(&["outdated"]), s(&["info", "wget"])] {
            let mut world = TestWorld::new();
            dispatch_quiet(&s(&["outdated"]), &world).unwrap();
            let state = world.cache_path.join("state.toml");
            let fresh = std::fs::read(&state).unwrap();
            std::fs::write(&state, "[[[").unwrap();
            world.github.refreshed.set(false);
            world.lock_wait = std::time::Duration::from_secs(10);
            let held =
                lock::acquire_waiting(&world.cache_path, &s(&["update"]), world.lock_wait).unwrap();
            let state_path = state.clone();
            let holder = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                std::fs::write(&state_path, fresh).unwrap();
                drop(held);
            });
            dispatch_quiet(&argv, &world).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            holder.join().unwrap();
            assert!(!world.github.refreshed.get(), "{argv:?} refreshed anyway");
        }
    }

    fn seed_untapped_leftovers(cache: &Path) {
        for rel in ["taps/gone/tap.git", "staging/taps/gone/tap/Formula"] {
            std::fs::create_dir_all(cache.join(rel)).unwrap();
        }
        let mut records = origin::OriginRecords::default();
        records.set(PkgKind::Formula, "stale", "gone/tap");
        records.save(cache).unwrap();
    }

    #[test]
    fn forced_refreshes_prune_untapped_leftovers() {
        for argv in [
            s(&["update", "-v"]),
            s(&["upgrade", "-v"]),
            s(&["outdated", "-v"]),
        ] {
            let world = TestWorld::new();
            seed_untapped_leftovers(&world.cache_path);
            dispatch_quiet(&argv, &world).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(!world.cache_path.join("taps/gone").exists(), "{argv:?}");
            assert!(
                !world.cache_path.join("staging/taps/gone").exists(),
                "{argv:?}"
            );
            let records = origin::OriginRecords::load(&world.cache_path);
            assert_eq!(records.get(PkgKind::Formula, "stale"), None, "{argv:?}");
        }
    }

    #[test]
    fn a_run_that_does_not_refresh_prunes_nothing() {
        let world = TestWorld::new();
        dispatch_quiet(&s(&["outdated"]), &world).unwrap();
        seed_untapped_leftovers(&world.cache_path);
        dispatch_quiet(&s(&["outdated"]), &world).unwrap();
        assert!(world.cache_path.join("taps/gone/tap.git").exists());
    }

    #[test]
    fn commands_that_never_touch_the_cache_ignore_the_lock() {
        for argv in [
            s(&["--version"]),
            s(&["help"]),
            s(&["settings"]),
            s(&["settings", "show"]),
            s(&["settings", "soak-hours", "48"]),
            s(&["services", "list"]),
        ] {
            let world = TestWorld::new();
            let _held = lock::acquire_waiting(
                &world.cache_path,
                &s(&["upgrade"]),
                std::time::Duration::from_millis(50),
            )
            .unwrap();
            dispatch_quiet(&argv, &world).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
    }

    #[test]
    fn dispatch_releases_the_lock_when_it_returns() {
        let world = TestWorld::new();
        dispatch_quiet(&s(&["outdated"]), &world).unwrap();
        let again = lock::acquire_waiting(
            &world.cache_path,
            &s(&["update"]),
            std::time::Duration::from_millis(50),
        );
        assert!(again.is_ok(), "{again:?}");
    }

    #[test]
    fn passthrough_services_is_exec_without_soak_flag() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["services", "start", "foo"]), &world).expect("dispatch") {
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
        match dispatch_quiet(
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
        assert!(
            !world.config_path().exists(),
            "passthrough must not persist --soak-hours"
        );
    }

    #[test]
    fn soak_hours_persists_on_soaked_command() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["--soak-hours", "48", "--version"]), &world).expect("version") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            !world.config_path().exists(),
            "version must not persist --soak-hours"
        );
        match dispatch_quiet(&s(&["--soak-hours", "48", "outdated"]), &world).expect("outdated") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        let text = std::fs::read_to_string(world.config_path()).expect("persisted config");
        assert_eq!(text, "SOAK_HOURS = 48\n");
    }

    #[test]
    fn dry_run_does_not_persist_soak_hours() {
        let world = TestWorld::new();
        let _ = dispatch_quiet(&s(&["--soak-hours", "48", "upgrade", "--dry-run"]), &world)
            .expect("dispatch");
        assert!(
            !world.config_path().exists(),
            "a dry run must not rewrite the user's config"
        );
    }

    #[test]
    fn outdated_empty_snapshots_refreshes() {
        let world = TestWorld::new();
        assert!(!world.cache_path.join("state.toml").exists());
        match dispatch_quiet(&s(&["outdated"]), &world).expect("dispatch") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            world.github.refreshed.get(),
            "refresh should have queried github"
        );
    }

    #[test]
    fn help_install_is_soak_aware() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["help", "install"]), &world).expect("help") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            !world.github.refreshed.get(),
            "help install must not refresh"
        );
        match dispatch_quiet(&s(&["help", "services"]), &world).expect("help services") {
            Dispatch::Exec(_bin, argv) => {
                assert_eq!(argv, s(&["help", "services"]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dispatch_writes_settings_output_to_the_injected_writer() {
        let world = TestWorld::new();
        let (result, out) = dispatch_captured(&s(&["settings", "soak-hours", "48"]), &world);
        assert!(matches!(result, Ok(Dispatch::Exit(0))), "{result:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&format!("wrote {}", world.config_path().display())),
            "{text:?}"
        );
        let (_, shown) = dispatch_captured(&s(&["settings", "show"]), &world);
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains("soak hours: 48"), "{shown:?}");
    }

    #[test]
    fn dispatch_writes_version_help_and_report_output_to_the_injected_writer() {
        let world = TestWorld::new();
        let (_, out) = dispatch_captured(&s(&["--version"]), &world);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}\n", cli::version_line())
        );
        let (_, out) = dispatch_captured(&s(&["--help"]), &world);
        assert_eq!(String::from_utf8(out).unwrap(), cli::help_text());
        let (_, out) = dispatch_captured(&s(&["help", "install"]), &world);
        assert!(!out.is_empty(), "command help lands in the writer");
        let (result, out) = dispatch_captured(&s(&["outdated"]), &world);
        assert!(matches!(result, Ok(Dispatch::Exit(0))), "{result:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("nothing outdated"), "{text:?}");
        let (_, out) = dispatch_captured(&s(&["outdated", "-v"]), &world);
        assert!(!out.is_empty());
    }

    #[test]
    fn version_and_help_exit_without_refresh() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["--version"]), &world).expect("version") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        match dispatch_quiet(&s(&["--help"]), &world).expect("help") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            !world.github.refreshed.get(),
            "version/help must not refresh snapshots"
        );
    }

    #[test]
    fn info_untapped_token_is_soak_aware_not_exec() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["info", "acme/tools/foo"]), &world).expect("dispatch") {
            Dispatch::Exit(1) => {}
            other => panic!("untapped tap token is refused, not exec'd: {other:?}"),
        }
    }

    #[test]
    fn outdated_ignores_names_and_does_not_exec() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["outdated", "acme/tools/foo"]), &world).expect("dispatch") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ensure_snapshots_refreshes_only_needed_taps() {
        let world = TestWorld::new();
        world.git.insert_commits(
            "https://github.com/hashicorp/homebrew-tap",
            &[
                ("hc2", now().unix_timestamp() - 3600),
                ("hc1", now().unix_timestamp() - 200 * 3600),
            ],
        );
        let brew = MockBrew {
            installed: vec![InstalledPkg {
                name: "terraform".into(),
                kind: PkgKind::Formula,
                receipt_rb: "class X < Formula\n  url \"https://e.com/terraform-1.0.0.tar.gz\"\n  sha256 \"a\"\nend\n".into(),
                pinned: false,
                tap: Some("hashicorp/tap".into()),
 staged_path: None,
}],
            taps: vec![
                TapInfo {
                    name: "hashicorp/tap".into(),
                    remote: Some("https://github.com/hashicorp/homebrew-tap".into()),
                },
                TapInfo {
                    name: "ericfitz/tap".into(),
                    remote: Some("https://github.com/ericfitz/homebrew-tap".into()),
                },
            ],
            ..MockBrew::new()
        };
        let world = TestWorld { brew, ..world };
        std::fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        std::fs::write(
            world.config_path(),
            "[[TAP]]\nname = \"hashicorp/tap\"\nsoak_hours = 100\n",
        )
        .unwrap();
        match dispatch_quiet(&s(&["update"]), &world).expect("update") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        let state = std::fs::read_to_string(world.cache_path().join("state.toml")).unwrap();
        assert!(state.contains("[taps.\"hashicorp/tap\"]"), "{state}");
        assert!(state.contains("hours = 100"), "{state}");
        assert!(
            !state.contains("ericfitz/tap"),
            "no installed soaked package: not refreshed\n{state}"
        );
    }

    fn assert_nothing_external_ran(world: &TestWorld, what: &str) {
        assert!(
            world.brew.runs.lock().unwrap().is_empty(),
            "{what}: brew ran"
        );
        assert!(
            world.brew.visible_runs.lock().unwrap().is_empty(),
            "{what}: brew ran visibly"
        );
        assert!(!world.github.refreshed.get(), "{what}: github was queried");
        assert!(
            !world.cache_path().exists(),
            "{what}: settings must not create the cache"
        );
    }

    fn config_dir_names(world: &TestWorld) -> Vec<String> {
        let dir = world.config_path();
        let dir = dir.parent().unwrap();
        let mut v: Vec<String> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        v.sort();
        v
    }

    #[test]
    fn settings_never_runs_brew_git_or_snapshots() {
        for args in [
            s(&["settings"]),
            s(&["settings", "show"]),
            s(&["settings", "soak-hours", "48"]),
            s(&["settings", "no-soak", "add", "wget"]),
            s(&["settings", "no-soak", "remove", "nope"]),
            s(&["settings", "tap-hours", "a/b", "5"]),
            s(&["settings", "tap-hours", "a/b", "--clear"]),
            s(&["settings", "repair"]),
            s(&["settings", "soak-hours", "--clear"]),
        ] {
            let world = TestWorld::new();
            match dispatch_quiet(&args, &world) {
                Ok(Dispatch::Exit(0)) => {}
                other => panic!("{args:?}: {other:?}"),
            }
            assert_nothing_external_ran(&world, &args.join(" "));
        }
    }

    #[test]
    fn settings_with_soak_hours_flag_is_usage_and_writes_nothing() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["--soak-hours", "1", "settings", "show"]), &world) {
            Err(Error::Usage(m)) => assert!(m.contains("settings soak-hours"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(!world.config_path().exists());
    }

    #[test]
    fn settings_edits_round_trip_and_show_reports_them() {
        let world = TestWorld::new();
        let path = world.config_path();
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::NoSoakAdd(s(&["WGet", "ericfitz/tap"]))),
            &path,
            None,
            now(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            format!(
                "added to NO_SOAK: wget\nadded to NO_SOAK: ericfitz/tap\nwrote {}\n",
                path.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "NO_SOAK = [\"wget\", \"ericfitz/tap\"]\n"
        );
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::TapHours {
                tap: "hashicorp/tap".into(),
                hours: Some(SoakHours::new(72).unwrap()),
            }),
            &path,
            None,
            now(),
            &mut out,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "NO_SOAK = [\"wget\", \"ericfitz/tap\"]\n\n[[TAP]]\nname = \"hashicorp/tap\"\nsoak_hours = 72\n"
        );
        let mut out = Vec::new();
        run_settings(&cli::SettingsCmd::Show, &path, Some("36"), now(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("soak hours: 36 (BREWSOAK_SOAK_HOURS)\n"),
            "{text}"
        );
        assert!(text.contains("  wget\n  ericfitz/tap\n"), "{text}");
        assert!(text.contains("  hashicorp/tap: 72 (own)\n"), "{text}");
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::NoSoakRemove(s(&["wget", "nope"]))),
            &path,
            None,
            now(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("removed from NO_SOAK: wget\nnot in NO_SOAK: nope\n"),
            "{text}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("NO_SOAK = [\"ericfitz/tap\"]\n"),
            "comment-free edit keeps the rest"
        );
    }

    #[test]
    fn settings_show_without_a_file_prints_the_defaults() {
        let world = TestWorld::new();
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Show,
            &world.config_path(),
            None,
            now(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("(no config file)\n"), "{text}");
        assert!(text.contains("soak hours: 24 (default)\n"), "{text}");
        assert!(!world.config_path().exists(), "show never writes");
    }

    #[test]
    fn settings_refuses_invalid_toml_and_leaves_it_alone() {
        let world = TestWorld::new();
        let path = world.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[[").unwrap();
        for cmd in [
            cli::SettingsCmd::Show,
            cli::SettingsCmd::Edit(cli::SettingsEdit::NoSoakAdd(s(&["wget"]))),
            cli::SettingsCmd::Edit(cli::SettingsEdit::SoakHours(None)),
        ] {
            let mut out = Vec::new();
            match run_settings(&cmd, &path, None, now(), &mut out) {
                Err(Error::Refusal(m)) => {
                    assert!(m.contains(&path.display().to_string()), "{cmd:?}: {m}");
                    assert!(m.contains("TOML parse error"), "{cmd:?}: {m}");
                }
                other => panic!("{cmd:?}: {other:?}"),
            }
            assert!(out.is_empty(), "{cmd:?}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[[");
        assert_eq!(
            config_dir_names(&world),
            ["config.toml"],
            "no backup, no temp file"
        );
    }

    #[test]
    fn settings_repair_writes_once_with_a_backup_then_nothing() {
        let world = TestWorld::new();
        let path = world.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# mine\nNO_SOAK = [\"wget\", \"a//b\", 3]\n").unwrap();
        let repair = s(&["settings", "repair"]);
        dispatch_quiet(&repair, &world).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nNO_SOAK = [\"wget\"]\n"
        );
        let names = config_dir_names(&world);
        assert_eq!(names.len(), 2, "{names:?}");
        let backup = names.iter().find(|n| n.ends_with(".bak")).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.parent().unwrap().join(backup)).unwrap(),
            "# mine\nNO_SOAK = [\"wget\", \"a//b\", 3]\n"
        );
        dispatch_quiet(&repair, &world).unwrap();
        assert_eq!(config_dir_names(&world), names, "second run writes nothing");
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::NoSoakRepair),
            &path,
            None,
            now(),
            &mut out,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "nothing to repair\n");
    }

    #[test]
    fn settings_repair_does_not_print_the_readers_warnings() {
        // Covered by edit_settings skipping them; the file is still fixed.
        let world = TestWorld::new();
        let path = world.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[TAP]]\nname = \"a/b\"\nNO_SOAK = []\n").unwrap();
        dispatch_quiet(&s(&["settings", "repair"]), &world).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[[TAP]]\nname = \"a/b\"\n"
        );
        dispatch_quiet(&s(&["settings", "repair"]), &world).unwrap();
        assert_eq!(
            config_dir_names(&world).len(),
            2,
            "second run writes nothing"
        );
    }

    #[test]
    fn settings_repair_refuses_invalid_toml_and_a_non_array() {
        let world = TestWorld::new();
        let path = world.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for contents in ["[[[", "NO_SOAK = 3\n"] {
            std::fs::write(&path, contents).unwrap();
            match dispatch_quiet(&s(&["settings", "repair"]), &world) {
                Err(Error::Refusal(_)) => {}
                other => panic!("{contents:?}: {other:?}"),
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
            assert_eq!(config_dir_names(&world), ["config.toml"]);
        }
    }

    #[test]
    fn settings_second_identical_edit_writes_nothing() {
        let world = TestWorld::new();
        let add = s(&["settings", "no-soak", "add", "wget"]);
        dispatch_quiet(&add, &world).unwrap();
        assert_eq!(config_dir_names(&world), ["config.toml"]);
        dispatch_quiet(&add, &world).unwrap();
        assert_eq!(
            config_dir_names(&world),
            ["config.toml"],
            "no-op edit: no write, no backup"
        );
        dispatch_quiet(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(
            config_dir_names(&world).len(),
            2,
            "a real edit leaves one backup"
        );
        dispatch_quiet(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(
            config_dir_names(&world).len(),
            2,
            "repeating it adds nothing"
        );
        let text = std::fs::read_to_string(world.config_path()).unwrap();
        assert_eq!(text, "NO_SOAK = [\"wget\"]\nSOAK_HOURS = 48\n");
    }

    #[test]
    fn settings_invalid_token_is_usage_and_writes_nothing() {
        let world = TestWorld::new();
        match dispatch_quiet(&s(&["settings", "no-soak", "add", "a/b/c/d"]), &world) {
            Err(Error::Usage(m)) => assert!(m.contains("a/b/c/d"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(!world.config_path().exists());
    }

    #[test]
    fn settings_clear_last_key_removes_the_file() {
        let world = TestWorld::new();
        dispatch_quiet(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert!(world.config_path().exists());
        let mut out = Vec::new();
        run_settings(
            &cli::SettingsCmd::Edit(cli::SettingsEdit::SoakHours(None)),
            &world.config_path(),
            None,
            now(),
            &mut out,
        )
        .unwrap();
        assert!(!world.config_path().exists(), "nothing left: file removed");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&format!(
                "removed {} (nothing left)",
                world.config_path().display()
            )),
            "{text}"
        );
        assert_eq!(config_dir_names(&world).len(), 1, "the backup remains");
    }

    #[test]
    fn soak_hours_persistence_keeps_comments() {
        let world = TestWorld::new();
        std::fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        std::fs::write(world.config_path(), "# keep me\nSOAK_HOURS = 6 # why\n").unwrap();
        match dispatch_quiet(&s(&["--soak-hours", "48", "outdated"]), &world).expect("outdated") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(world.config_path()).unwrap(),
            "# keep me\nSOAK_HOURS = 48 # why\n"
        );
    }

    #[test]
    fn settings_soak_hours_still_writes_when_env_is_set() {
        let world = TestWorld {
            env_soak: Some("36".into()),
            ..TestWorld::new()
        };
        dispatch_quiet(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(
            std::fs::read_to_string(world.config_path()).unwrap(),
            "SOAK_HOURS = 48\n",
            "the warning (stderr) does not stop the edit"
        );
    }
}
