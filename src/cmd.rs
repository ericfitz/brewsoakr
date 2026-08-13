use crate::brew::{Brew, InstalledPkg};
use crate::eligibility::{self, DesiredAction};
use crate::git::GitStore;
use crate::github::GithubApi;
use crate::identity::{self, PkgIdentity};
use crate::resolve::{self, PkgKind, PkgRef};
use crate::snapshot::{self, Snapshots};
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

struct ResolvedView {
    installed: Option<PkgIdentity>,
    cutoff: Option<PkgIdentity>,
    head: Option<PkgIdentity>,
    action: DesiredAction,
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
    let repo = tap_repo(cache, kind);
    let tap = match kind {
        PkgKind::Formula => &snaps.core,
        PkgKind::Cask => &snaps.cask,
    };
    let pkg = PkgRef {
        name: name.to_string(),
        kind,
    };
    let blobs = resolve::resolve_blobs(git, &repo, &tap.cutoff_sha, &tap.head_sha, &pkg)?;
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
    })
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
}
