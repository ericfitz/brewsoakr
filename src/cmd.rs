use crate::brew::{Brew, InstalledPkg};
use crate::eligibility::{self, DesiredAction, UpstreamStatus};
use crate::git::GitStore;
use crate::github::GithubApi;
use crate::identity::{self, PkgIdentity};
use crate::resolve::{self, PkgKind, PkgRef};
use crate::snapshot::{self, Snapshots};
use crate::tap;
use crate::{Error, SoakHours};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn refusal_message(action: DesiredAction, name: &str, brew_verb: &str) -> Option<String> {
    let why = match action {
        DesiredAction::RefuseTooNew => format!("{name} is too new (born inside the soak window)"),
        DesiredAction::RefuseYanked => format!("{name} is missing at HEAD (yanked)"),
        DesiredAction::RefuseDeprecated => format!("{name} is deprecated or disabled at HEAD"),
        DesiredAction::NoOpAlreadySoaked
        | DesiredAction::LeaveAheadOfSoak
        | DesiredAction::InstallCutoff => return None,
    };
    Some(format!(
        "{why}; use `brew {brew_verb} {name}` to bypass brewsoakr."
    ))
}

pub fn ahead_message(name: &str) -> String {
    format!("{name} is ahead of soak; leaving installed artifact unchanged")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub refused: bool,
    pub brew_status: Option<i32>,
}

pub fn combine_exit(refused: bool, brew_status: Option<i32>) -> i32 {
    let brew = brew_status.unwrap_or(0);
    if brew > 1 {
        brew
    } else if refused {
        1
    } else {
        brew
    }
}

pub fn update(
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    hours: SoakHours,
    now: time::OffsetDateTime,
    out: &mut impl Write,
) -> Result<(), Error> {
    let snaps = snapshot::refresh(git, gh, cache, hours, now)?;
    writeln!(out, "soak hours: {}", snaps.hours.get())?;
    writeln!(out, "core cutoff: {}", snaps.core.cutoff_sha)?;
    writeln!(out, "core head: {}", snaps.core.head_sha)?;
    writeln!(out, "cask cutoff: {}", snaps.cask.cutoff_sha)?;
    writeln!(out, "cask head: {}", snaps.cask.head_sha)?;
    writeln!(out, "snapshots refreshed")?;
    Ok(())
}

pub fn outdated(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let installed = brew.installed_core()?;
    let mut upgrades = Vec::new();
    let mut held = Vec::new();
    let mut ahead = Vec::new();
    for pkg in &installed {
        let view = resolve_view(
            git,
            snaps,
            cache,
            &pkg.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
        )?;
        match view.action {
            DesiredAction::InstallCutoff => {
                let installed_ver = view
                    .installed
                    .as_ref()
                    .map(identity_version)
                    .unwrap_or("unknown");
                let cutoff_ver = view.cutoff.as_ref().map(identity_version).unwrap_or("none");
                upgrades.push(format!("{} ({installed_ver}) < {cutoff_ver}", pkg.name));
            }
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => {
                if let Some(why) = hold_why(view.action) {
                    held.push(format!("{}: {why}", pkg.name));
                }
            }
            DesiredAction::LeaveAheadOfSoak => ahead.push(pkg.name.clone()),
            DesiredAction::NoOpAlreadySoaked => {}
        }
    }
    write_section(out, "==> Outdated (will upgrade)", &upgrades)?;
    write_section(out, "==> Held", &held)?;
    write_section(out, "==> Ahead of soak", &ahead)?;
    Ok(RunResult {
        refused: false,
        brew_status: None,
    })
}

pub fn info(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    names: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let installed = brew.installed_core()?;
    for name in names {
        let view = resolve_named(git, snaps, cache, name, &installed)?;
        writeln!(out, "{name}")?;
        writeln!(
            out,
            "installed: {}",
            view.installed
                .as_ref()
                .map(identity_version)
                .unwrap_or("not installed")
        )?;
        writeln!(
            out,
            "cutoff: {}",
            view.cutoff.as_ref().map(identity_version).unwrap_or("none")
        )?;
        writeln!(
            out,
            "head: {}",
            view.head.as_ref().map(identity_version).unwrap_or("none")
        )?;
        writeln!(out, "action: {}", action_label(view.action))?;
    }
    Ok(RunResult {
        refused: false,
        brew_status: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn upgrade(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let installed = brew.installed_core()?;
    let targets: Vec<String> = if names.is_empty() {
        installed.iter().map(|p| p.name.clone()).collect()
    } else {
        names.to_vec()
    };
    apply_many(
        brew, git, snaps, cache, tap_root, &targets, "upgrade", false, false, user_flags,
        &installed, out,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    force_cask: bool,
    force_formula: bool,
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    if names.is_empty() {
        return Err(Error::Usage("install: no packages specified".into()));
    }
    let installed = brew.installed_core()?;
    apply_many(
        brew,
        git,
        snaps,
        cache,
        tap_root,
        names,
        "install",
        force_cask,
        force_formula,
        user_flags,
        &installed,
        out,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_many(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    names: &[String],
    brew_verb: &str,
    force_cask: bool,
    force_formula: bool,
    user_flags: &[String],
    installed: &[InstalledPkg],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let mut session = ApplySession {
        brew,
        git,
        snaps,
        cache,
        tap_root,
        user_flags,
        installed,
        brew_verb,
        force_cask,
        force_formula,
        tapped: false,
        refused: false,
        brew_status: None,
        out,
    };
    for name in names {
        session.apply_one(name)?;
    }
    Ok(RunResult {
        refused: session.refused,
        brew_status: session.brew_status,
    })
}

struct ApplySession<'a, B, G, W> {
    brew: &'a B,
    git: &'a G,
    snaps: &'a Snapshots,
    cache: &'a Path,
    tap_root: &'a Path,
    user_flags: &'a [String],
    installed: &'a [InstalledPkg],
    brew_verb: &'a str,
    force_cask: bool,
    force_formula: bool,
    tapped: bool,
    refused: bool,
    brew_status: Option<i32>,
    out: &'a mut W,
}

impl<B: Brew, G: GitStore, W: Write> ApplySession<'_, B, G, W> {
    fn apply_one(&mut self, name: &str) -> Result<(), Error> {
        if resolve::is_third_party(name) {
            let mut args = vec![self.brew_verb.to_string()];
            args.extend(self.user_flags.iter().cloned());
            args.push(name.to_string());
            self.record_run(&args)?;
            return Ok(());
        }

        let kind = self.resolve_kind(name)?;
        let receipt = self
            .installed
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.receipt_rb.as_str());
        let view = resolve_view(self.git, self.snaps, self.cache, name, kind, receipt)?;
        match view.action {
            DesiredAction::NoOpAlreadySoaked => {
                if self.brew_verb == "install" {
                    writeln!(self.out, "{name} is already installed")?;
                } else {
                    writeln!(self.out, "{name} is already soaked")?;
                }
            }
            DesiredAction::LeaveAheadOfSoak => {
                writeln!(self.out, "{}", ahead_message(name))?;
            }
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => {
                if let Some(msg) = refusal_message(view.action, name, self.brew_verb) {
                    writeln!(self.out, "{msg}")?;
                }
                self.refused = true;
            }
            DesiredAction::InstallCutoff => {
                self.install_cutoff(name, kind, &view)?;
            }
        }
        Ok(())
    }

    fn install_cutoff(
        &mut self,
        name: &str,
        kind: PkgKind,
        view: &ResolvedView,
    ) -> Result<(), Error> {
        let pkg = PkgRef {
            name: name.to_string(),
            kind,
        };
        let blob = view.cutoff_blob.as_deref().ok_or_else(|| {
            Error::Other(format!("{name} is eligible but the cutoff blob is missing"))
        })?;
        tap::write_blob(self.tap_root, &pkg, blob)?;

        let deps = tap::dep_closure(self.brew, kind, name, |dep| {
            cutoff_blob_exists(self.git, self.snaps, self.cache, dep, kind).unwrap_or(false)
        })?;
        for dep in deps {
            if self.installed.iter().any(|p| p.name == dep) {
                continue;
            }
            if !self.install_missing_dep(name, kind, &dep)? {
                return Ok(());
            }
        }

        self.ensure_tap()?;
        let args = tap::brew_install_args(&pkg, self.user_flags, true);
        self.record_run(&args)
    }

    /// Install a missing cutoff dep. Returns false when the target was refused.
    fn install_missing_dep(
        &mut self,
        target: &str,
        kind: PkgKind,
        dep: &str,
    ) -> Result<bool, Error> {
        let blobs = resolve_pkg_blobs(self.git, self.snaps, self.cache, dep, kind)?;
        let status = eligibility::upstream_status(blobs.cutoff.as_deref(), blobs.head.as_deref());
        if status != UpstreamStatus::Eligible {
            self.refuse_ineligible_dep(target, dep, status)?;
            return Ok(false);
        }
        let blob = blobs.cutoff.as_deref().ok_or_else(|| {
            Error::Other(format!("{dep} is eligible but the cutoff blob is missing"))
        })?;
        let pkg = PkgRef {
            name: dep.to_string(),
            kind,
        };
        tap::write_blob(self.tap_root, &pkg, blob)?;
        self.ensure_tap()?;
        let args = tap::brew_install_args(&pkg, &[], false);
        self.record_run(&args)?;
        Ok(true)
    }

    fn refuse_ineligible_dep(
        &mut self,
        target: &str,
        dep: &str,
        status: UpstreamStatus,
    ) -> Result<(), Error> {
        self.refused = true;
        let why = match status {
            UpstreamStatus::TooNew => {
                format!("{dep} is too new (born inside the soak window)")
            }
            UpstreamStatus::Yanked => format!("{dep} is missing at HEAD (yanked)"),
            UpstreamStatus::Deprecated => {
                format!("{dep} is deprecated or disabled at HEAD")
            }
            UpstreamStatus::Eligible => return Ok(()),
        };
        writeln!(
            self.out,
            "cannot install {target}: dependency {why}; use `brew {} {target}` to bypass brewsoakr.",
            self.brew_verb
        )?;
        Ok(())
    }

    fn resolve_kind(&self, name: &str) -> Result<PkgKind, Error> {
        if self.force_cask {
            return Ok(PkgKind::Cask);
        }
        if self.force_formula {
            return Ok(PkgKind::Formula);
        }
        if let Some(pkg) = self.installed.iter().find(|p| p.name == name) {
            return Ok(pkg.kind);
        }
        let formula = resolve_view(
            self.git,
            self.snaps,
            self.cache,
            name,
            PkgKind::Formula,
            None,
        )?;
        if formula.cutoff.is_some() || formula.head.is_some() {
            Ok(PkgKind::Formula)
        } else {
            Ok(PkgKind::Cask)
        }
    }

    fn ensure_tap(&mut self) -> Result<(), Error> {
        if !self.tapped {
            self.brew.tap_new_soaked()?;
            self.tapped = true;
        }
        Ok(())
    }

    fn record_run(&mut self, args: &[String]) -> Result<(), Error> {
        let output = self.brew.run(args)?;
        let code = output.status.code().unwrap_or(1);
        self.brew_status = Some(match self.brew_status {
            Some(prev) => prev.max(code),
            None => code,
        });
        Ok(())
    }
}

struct ResolvedView {
    installed: Option<PkgIdentity>,
    cutoff: Option<PkgIdentity>,
    head: Option<PkgIdentity>,
    action: DesiredAction,
    cutoff_blob: Option<Vec<u8>>,
}

fn resolve_named(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    installed: &[InstalledPkg],
) -> Result<ResolvedView, Error> {
    if let Some(pkg) = installed.iter().find(|p| p.name == name) {
        return resolve_view(git, snaps, cache, name, pkg.kind, Some(&pkg.receipt_rb));
    }
    let formula = resolve_view(git, snaps, cache, name, PkgKind::Formula, None)?;
    if formula.cutoff.is_some() || formula.head.is_some() {
        return Ok(formula);
    }
    resolve_view(git, snaps, cache, name, PkgKind::Cask, None)
}

fn resolve_view(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    kind: PkgKind,
    receipt_rb: Option<&str>,
) -> Result<ResolvedView, Error> {
    let blobs = resolve_pkg_blobs(git, snaps, cache, name, kind)?;
    let installed = match receipt_rb {
        Some(rb) => Some(parse_pkg(kind, rb)?),
        None => None,
    };
    let cutoff = match blobs.cutoff.as_deref() {
        Some(bytes) => Some(parse_pkg_bytes(kind, bytes)?),
        None => None,
    };
    let head = match blobs.head.as_deref() {
        Some(bytes) => Some(parse_pkg_bytes(kind, bytes)?),
        None => None,
    };
    let status = eligibility::upstream_status(blobs.cutoff.as_deref(), blobs.head.as_deref());
    let action =
        eligibility::desired_action(status, installed.as_ref(), cutoff.as_ref(), head.as_ref());
    Ok(ResolvedView {
        installed,
        cutoff,
        head,
        action,
        cutoff_blob: blobs.cutoff,
    })
}

fn resolve_pkg_blobs(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    kind: PkgKind,
) -> Result<resolve::ResolvedBlobs, Error> {
    let repo = tap_repo(cache, kind);
    let tap = match kind {
        PkgKind::Formula => &snaps.core,
        PkgKind::Cask => &snaps.cask,
    };
    let pkg = PkgRef {
        name: name.to_string(),
        kind,
    };
    resolve::resolve_blobs(git, &repo, &tap.cutoff_sha, &tap.head_sha, &pkg)
}

fn cutoff_blob_exists(
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    name: &str,
    kind: PkgKind,
) -> Result<bool, Error> {
    Ok(resolve_pkg_blobs(git, snaps, cache, name, kind)?
        .cutoff
        .is_some())
}

fn tap_repo(cache: &Path, kind: PkgKind) -> PathBuf {
    match kind {
        PkgKind::Formula => cache.join("core.git"),
        PkgKind::Cask => cache.join("cask.git"),
    }
}

fn parse_pkg(kind: PkgKind, rb: &str) -> Result<PkgIdentity, Error> {
    match kind {
        PkgKind::Formula => Ok(PkgIdentity::Formula(identity::parse_formula(rb)?)),
        PkgKind::Cask => Ok(PkgIdentity::Cask(identity::parse_cask(rb)?)),
    }
}

fn parse_pkg_bytes(kind: PkgKind, bytes: &[u8]) -> Result<PkgIdentity, Error> {
    let rb = std::str::from_utf8(bytes)
        .map_err(|_| Error::Other("package blob is not valid UTF-8".into()))?;
    parse_pkg(kind, rb)
}

fn identity_version(id: &PkgIdentity) -> &str {
    match id {
        PkgIdentity::Formula(f) => f.version.as_str(),
        PkgIdentity::Cask(c) => c.version.as_str(),
    }
}

fn action_label(action: DesiredAction) -> &'static str {
    match action {
        DesiredAction::InstallCutoff => "install cutoff",
        DesiredAction::NoOpAlreadySoaked => "already soaked",
        DesiredAction::LeaveAheadOfSoak => "ahead of soak",
        DesiredAction::RefuseTooNew => "too new",
        DesiredAction::RefuseYanked => "yanked",
        DesiredAction::RefuseDeprecated => "deprecated",
    }
}

fn hold_why(action: DesiredAction) -> Option<&'static str> {
    match action {
        DesiredAction::RefuseTooNew => Some("too new (born inside the soak window)"),
        DesiredAction::RefuseYanked => Some("missing at HEAD (yanked)"),
        DesiredAction::RefuseDeprecated => Some("deprecated or disabled at HEAD"),
        _ => None,
    }
}

fn write_section(out: &mut impl Write, header: &str, lines: &[String]) -> Result<(), Error> {
    if lines.is_empty() {
        return Ok(());
    }
    writeln!(out, "{header}")?;
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::{InstalledPkg, MockBrew};
    use crate::eligibility::DesiredAction;
    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};
    use crate::resolve::PkgKind;
    use crate::snapshot::TapSnapshot;
    use std::path::Path;
    use time::{Duration, OffsetDateTime};

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

    fn assert_refuse_copy(action: DesiredAction, name: &str, brew_verb: &str) {
        let msg = refusal_message(action, name, brew_verb).expect("refuse message");
        assert!(
            msg.contains(&format!("brew {brew_verb} {name}")),
            "{action:?} missing `brew {brew_verb} {name}`: {msg}"
        );
        assert!(
            !msg.contains("--now"),
            "{action:?} must not mention --now: {msg}"
        );
    }

    #[test]
    fn refuse_too_new_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseTooNew, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseTooNew, "wget", "upgrade");
    }

    #[test]
    fn refuse_yanked_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseYanked, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseYanked, "wget", "upgrade");
    }

    #[test]
    fn refuse_deprecated_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseDeprecated, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseDeprecated, "wget", "upgrade");
    }

    #[test]
    fn leave_ahead_of_soak_is_not_a_refusal() {
        assert_eq!(
            refusal_message(DesiredAction::LeaveAheadOfSoak, "wget", "upgrade"),
            None
        );
    }

    #[test]
    fn combine_exit_refused_with_ok_brew_is_1() {
        assert_eq!(combine_exit(true, Some(0)), 1);
    }

    #[test]
    fn combine_exit_success_is_0() {
        assert_eq!(combine_exit(false, Some(0)), 0);
    }

    #[test]
    fn combine_exit_brew_gt_1_wins() {
        assert_eq!(combine_exit(true, Some(3)), 3);
    }

    #[test]
    fn update_writes_shas_and_creates_state_toml() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let hours = SoakHours::new(24).expect("hours >= 1");
        let mut out = Vec::new();
        update(&git, &fixture_gh(), dir.path(), hours, now(), &mut out).expect("update");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("thirtyh"), "{text}");
        assert!(text.contains("headsha"), "{text}");
        assert!(
            dir.path().join("state.toml").is_file(),
            "state.toml missing"
        );
    }

    fn unused_cache() -> &'static Path {
        Path::new("/brewsoak-in-memory-unused")
    }

    fn formula_rb(name: &str, version: &str, sha: &str) -> String {
        format!(
            "class X < Formula\n  url \"https://example.com/{name}-{version}.tar.gz\"\n  sha256 \"{sha}\"\nend\n"
        )
    }

    fn view_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let alpha_old = formula_rb("alpha", "1.0.0", "oldsha");
        let alpha_mid = formula_rb("alpha", "1.1.0", "midsha");
        let alpha_new = formula_rb("alpha", "1.2.0", "newsha");
        let beta_old = formula_rb("beta", "1.0.0", "oldsha");
        let beta_new = formula_rb("beta", "1.2.0", "newsha");
        let gamma_mid = formula_rb("gamma", "1.1.0", "midsha");
        let gamma_new = formula_rb("gamma", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", alpha_mid);
        git.insert_blob("headsha", "Formula/a/alpha.rb", alpha_new);
        git.insert_blob("headsha", "Formula/b/beta.rb", beta_new);
        git.insert_blob("cutoffsha", "Formula/g/gamma.rb", gamma_mid);
        git.insert_blob("headsha", "Formula/g/gamma.rb", gamma_new.clone());

        let brew = MockBrew {
            installed: vec![
                InstalledPkg {
                    name: "alpha".into(),
                    kind: PkgKind::Formula,
                    receipt_rb: alpha_old,
                },
                InstalledPkg {
                    name: "beta".into(),
                    kind: PkgKind::Formula,
                    receipt_rb: beta_old,
                },
                InstalledPkg {
                    name: "gamma".into(),
                    kind: PkgKind::Formula,
                    receipt_rb: gamma_new,
                },
            ],
            ..MockBrew::new()
        };
        let snaps = Snapshots {
            core: TapSnapshot {
                cutoff_sha: "cutoffsha".into(),
                head_sha: "headsha".into(),
            },
            cask: TapSnapshot {
                cutoff_sha: "caskcut".into(),
                head_sha: "caskhead".into(),
            },
            hours: SoakHours::new(24).expect("hours >= 1"),
        };
        (brew, git, snaps)
    }

    #[test]
    fn outdated_lists_upgrade_held_and_ahead_sections() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let result = outdated(&brew, &git, &snaps, unused_cache(), &mut out).expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("==> Outdated (will upgrade)"),
            "missing outdated header: {text}"
        );
        assert!(text.contains("==> Held"), "missing held header: {text}");
        assert!(
            text.contains("==> Ahead of soak"),
            "missing ahead header: {text}"
        );
        assert!(text.contains("alpha"), "missing alpha: {text}");
        assert!(text.contains("beta"), "missing beta: {text}");
        assert!(text.contains("gamma"), "missing gamma: {text}");
        assert!(!result.refused, "listing holds is not a refusal");
    }

    #[test]
    fn info_mentions_cutoff_version_and_install_cutoff() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let names = ["alpha".to_string()];
        let result = info(&brew, &git, &snaps, unused_cache(), &names, &mut out).expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("1.1.0"), "missing cutoff version: {text}");
        assert!(
            text.contains("install cutoff") || text.to_ascii_lowercase().contains("upgrade"),
            "missing install cutoff / upgrade wording: {text}"
        );
        assert!(!result.refused, "info is read-only");
    }

    fn core_snaps() -> Snapshots {
        Snapshots {
            core: TapSnapshot {
                cutoff_sha: "cutoffsha".into(),
                head_sha: "headsha".into(),
            },
            cask: TapSnapshot {
                cutoff_sha: "caskcut".into(),
                head_sha: "caskhead".into(),
            },
            hours: SoakHours::new(24).expect("hours >= 1"),
        }
    }

    fn formula_pkg(name: &str, receipt_rb: String) -> InstalledPkg {
        InstalledPkg {
            name: name.into(),
            kind: PkgKind::Formula,
            receipt_rb,
        }
    }

    fn lock_runs(brew: &MockBrew) -> Vec<Vec<String>> {
        brew.runs.lock().expect("runs").clone()
    }

    fn run_has_token(runs: &[Vec<String>], token: &str) -> bool {
        runs.iter().any(|args| args.iter().any(|a| a == token))
    }

    fn run_is_soaked_install(runs: &[Vec<String>], name: &str) -> bool {
        let token = format!("brewsoakr/soaked/{name}");
        runs.iter().any(|args| {
            args.first().map(String::as_str) == Some("install")
                && args.iter().any(|a| a == &token)
                && args.iter().any(|a| a == "--ignore-dependencies")
        })
    }

    #[test]
    fn upgrade_mixed_applies_eligible_and_refuses_too_new() {
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let ok_new = formula_rb("ok", "1.2.0", "newsha");
        let new_old = formula_rb("new", "1.0.0", "oldsha");
        let new_head = formula_rb("new", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob("headsha", "Formula/o/ok.rb", ok_new);
        git.insert_blob("headsha", "Formula/n/new.rb", new_head);

        let brew = MockBrew {
            installed: vec![formula_pkg("ok", ok_old), formula_pkg("new", new_old)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "ok"),
            "expected soaked install of ok with --ignore-dependencies: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "new") && !run_has_token(&runs, "brewsoakr/soaked/new"),
            "must not install too-new package: {runs:?}"
        );
        assert!(result.refused, "mixed upgrade must refuse the too-new pkg");
    }

    #[test]
    fn upgrade_ahead_of_soak_is_not_a_refusal() {
        let ahead_mid = formula_rb("ahead", "1.1.0", "midsha");
        let ahead_new = formula_rb("ahead", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/ahead.rb", ahead_mid);
        git.insert_blob("headsha", "Formula/a/ahead.rb", ahead_new.clone());

        let brew = MockBrew {
            installed: vec![formula_pkg("ahead", ahead_new)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        let runs = lock_runs(&brew);
        assert!(!result.refused, "ahead of soak is not a refusal");
        assert!(
            !runs
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("install")),
            "ahead of soak must not install: {runs:?}"
        );
        assert!(
            text.contains(&ahead_message("ahead")),
            "missing ahead message: {text}"
        );
    }

    #[test]
    fn install_fresh_eligible_runs_install() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);

        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "fresh"),
            "expected soaked install of fresh: {runs:?}"
        );
        assert!(!result.refused, "eligible fresh install must not refuse");
    }

    #[test]
    fn install_fresh_too_new_refuses() {
        let fresh_head = formula_rb("fresh", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_head);

        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let text = String::from_utf8(out).expect("utf8");
        let runs = lock_runs(&brew);
        assert!(
            !runs
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("install")),
            "too-new install must not run brew install: {runs:?}"
        );
        assert!(
            text.contains("brew install fresh"),
            "refusal must mention brew install fresh: {text}"
        );
        assert!(result.refused, "too-new install must refuse");
    }

    #[test]
    fn install_refuses_target_when_dep_yanked() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let lib_mid = formula_rb("lib", "1.0.0", "libsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        git.insert_blob("cutoffsha", "Formula/l/lib.rb", lib_mid);

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("fresh".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/fresh"),
            "yanked dep must refuse target install: {runs:?}"
        );
        assert!(result.refused, "missing yanked dep refuses the target");
    }

    #[test]
    fn upgrade_third_party_passthrough() {
        let brew = MockBrew::new();
        let git = InMemoryGit::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["acme/tools/foo".to_string()];
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &names,
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_has_token(&runs, "acme/tools/foo"),
            "third-party must brew.run original name: {runs:?}"
        );
        assert!(
            !runs
                .iter()
                .any(|args| args.iter().any(|a| a.contains("brewsoakr/soaked"))),
            "third-party must not use soaked tap: {runs:?}"
        );
        let _ = result;
    }
}
