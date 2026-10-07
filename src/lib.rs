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
pub mod nosoak;
pub mod origin;
pub mod paths;
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
    match dispatch(args, &RealWorld::new()) {
        Ok(Dispatch::Exit(c)) => c,
        Ok(Dispatch::Exec(bin, argv)) => brew::exec(&bin, &argv),
        Err(e) => {
            eprintln!("brewsoak: {e}");
            e.exit_code()
        }
    }
}

pub fn dispatch(args: &[String], world: &impl World) -> Result<Dispatch, Error> {
    paths::warn_duplicate_install();
    let inv = cli::parse_argv(args)?;
    world.brew().set_raw(inv.raw);
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
            &mut std::io::stdout(),
        )?;
        return Ok(Dispatch::Exit(0));
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
                println!("{note}");
            }
        }
    }

    match inv.command {
        cli::Command::Version => {
            println!("{}", cli::version_line());
            Ok(Dispatch::Exit(0))
        }
        cli::Command::Help { topic: None } => {
            print!("{}", cli::help_text());
            Ok(Dispatch::Exit(0))
        }
        cli::Command::Help { topic: Some(topic) } => {
            if let Some(text) = cli::command_help(&topic) {
                print!("{text}");
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
            let mut out = std::io::stdout();
            soaked_exit(
                cmd::update(
                    world.brew(),
                    world.git(),
                    world.github(),
                    &cache,
                    &cfg,
                    &pkgs,
                    world.now(),
                    cmd::is_verbose(&inv.brew_args),
                    &mut out,
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
            let mut out = std::io::stdout();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &[],
                world.now(),
                false,
                &mut out,
            )?;
            soaked_exit(cmd::outdated(
                world.brew(),
                world.git(),
                &snaps,
                &cache,
                &pkgs,
                &cfg,
                &inv.brew_args,
                &mut out,
            ))
        }
        cli::Command::Info { names } => {
            let cache = world.cache_path();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let mut out = std::io::stdout();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &cmd::explicit_tap_tokens(&names),
                world.now(),
                false,
                &mut out,
            )?;
            soaked_exit(cmd::info(
                world.git(),
                &snaps,
                &cache,
                &pkgs,
                &cfg,
                &names,
                &inv.brew_args,
                &mut out,
            ))
        }
        cli::Command::Upgrade { names } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let mut out = std::io::stdout();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &cmd::explicit_tap_tokens(&names),
                world.now(),
                true,
                &mut out,
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
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let mut out = std::io::stdout();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &cmd::explicit_tap_tokens(&names),
                world.now(),
                true,
                &mut out,
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
                &mut out,
            ))
        }
        cli::Command::Reinstall { names } => {
            let cache = world.cache_path();
            let tap_root = world.tap_root();
            let pkgs = inventory::Inventory::load(world.brew(), &cache, &cfg)?;
            let mut out = std::io::stdout();
            let snaps = cmd::ensure_snapshots(
                world.git(),
                world.github(),
                &cache,
                &cfg,
                &pkgs,
                &cmd::explicit_tap_tokens(&names),
                world.now(),
                true,
                &mut out,
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
                &mut out,
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
    for warning in &config::parse_file(&doc.to_string()).warnings {
        eprintln!("brewsoak: warning: {warning}");
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
        assert!(
            !world.config_path().exists(),
            "passthrough must not persist --soak-hours"
        );
    }

    #[test]
    fn soak_hours_persists_on_soaked_command() {
        let world = TestWorld::new();
        match dispatch(&s(&["--soak-hours", "48", "--version"]), &world).expect("version") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            !world.config_path().exists(),
            "version must not persist --soak-hours"
        );
        match dispatch(&s(&["--soak-hours", "48", "outdated"]), &world).expect("outdated") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        let text = std::fs::read_to_string(world.config_path()).expect("persisted config");
        assert_eq!(text, "SOAK_HOURS = 48\n");
    }

    #[test]
    fn dry_run_does_not_persist_soak_hours() {
        let world = TestWorld::new();
        let _ = dispatch(&s(&["--soak-hours", "48", "upgrade", "--dry-run"]), &world)
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
        match dispatch(&s(&["outdated"]), &world).expect("dispatch") {
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
        match dispatch(&s(&["help", "install"]), &world).expect("help") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            !world.github.refreshed.get(),
            "help install must not refresh"
        );
        match dispatch(&s(&["help", "services"]), &world).expect("help services") {
            Dispatch::Exec(_bin, argv) => {
                assert_eq!(argv, s(&["help", "services"]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn version_and_help_exit_without_refresh() {
        let world = TestWorld::new();
        match dispatch(&s(&["--version"]), &world).expect("version") {
            Dispatch::Exit(0) => {}
            other => panic!("{other:?}"),
        }
        match dispatch(&s(&["--help"]), &world).expect("help") {
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
        match dispatch(&s(&["info", "acme/tools/foo"]), &world).expect("dispatch") {
            Dispatch::Exit(1) => {}
            other => panic!("untapped tap token is refused, not exec'd: {other:?}"),
        }
    }

    #[test]
    fn outdated_ignores_names_and_does_not_exec() {
        let world = TestWorld::new();
        match dispatch(&s(&["outdated", "acme/tools/foo"]), &world).expect("dispatch") {
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
        match dispatch(&s(&["update"]), &world).expect("update") {
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
            s(&["settings", "soak-hours", "--clear"]),
        ] {
            let world = TestWorld::new();
            match dispatch(&args, &world) {
                Ok(Dispatch::Exit(0)) => {}
                other => panic!("{args:?}: {other:?}"),
            }
            assert_nothing_external_ran(&world, &args.join(" "));
        }
    }

    #[test]
    fn settings_with_soak_hours_flag_is_usage_and_writes_nothing() {
        let world = TestWorld::new();
        match dispatch(&s(&["--soak-hours", "1", "settings", "show"]), &world) {
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
    fn settings_second_identical_edit_writes_nothing() {
        let world = TestWorld::new();
        let add = s(&["settings", "no-soak", "add", "wget"]);
        dispatch(&add, &world).unwrap();
        assert_eq!(config_dir_names(&world), ["config.toml"]);
        dispatch(&add, &world).unwrap();
        assert_eq!(
            config_dir_names(&world),
            ["config.toml"],
            "no-op edit: no write, no backup"
        );
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(
            config_dir_names(&world).len(),
            2,
            "a real edit leaves one backup"
        );
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
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
        match dispatch(&s(&["settings", "no-soak", "add", "a/b/c/d"]), &world) {
            Err(Error::Usage(m)) => assert!(m.contains("a/b/c/d"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(!world.config_path().exists());
    }

    #[test]
    fn settings_clear_last_key_removes_the_file() {
        let world = TestWorld::new();
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
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
        match dispatch(&s(&["--soak-hours", "48", "outdated"]), &world).expect("outdated") {
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
        dispatch(&s(&["settings", "soak-hours", "48"]), &world).unwrap();
        assert_eq!(
            std::fs::read_to_string(world.config_path()).unwrap(),
            "SOAK_HOURS = 48\n",
            "the warning (stderr) does not stop the edit"
        );
    }
}
