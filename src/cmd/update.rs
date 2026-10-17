use super::bare::{bare_action, snapshotted_pkgs};
use super::notes::{indented_detail, write_section, write_session_tail};
use super::snapshots::refresh_plan;
use super::view::{prefetch_orderings, resolve_pkg_blobs, resolve_view};
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::eligibility::DesiredAction;
use crate::flags;
use crate::git::GitStore;
use crate::github::GithubApi;
use crate::identity::PkgIdentity;
use crate::inventory::Inventory;
use crate::lock::RunLock;
use crate::prune;
use crate::report::{self};
use crate::snapshot::{self, Snapshots};
use std::io::Write;
use std::path::Path;

pub(super) type Classified = (
    String,
    DesiredAction,
    Option<PkgIdentity>,
    Option<PkgIdentity>,
    Option<PkgIdentity>,
);

pub(super) fn classify_installed(
    brew: &impl Brew,
    git: &impl GitStore,
    cache: &Path,
    snaps: &Snapshots,
    inv: &Inventory,
) -> Result<Vec<Classified>, Error> {
    let mut out = Vec::new();
    prefetch_orderings(brew, git, snaps, cache, inv);
    for pkg in snapshotted_pkgs(inv, snaps) {
        let Some(mut view) = resolve_view(
            brew,
            git,
            snaps,
            cache,
            &pkg.origin,
            &pkg.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
        )?
        else {
            continue;
        };
        // `brew update` has no greedy mode; the summary reads like a bare run.
        view.action = bare_action(&view, flags::Greedy::default());
        out.push((
            pkg.name.clone(),
            view.action,
            view.installed,
            view.cutoff,
            view.head,
        ));
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_update_summary(
    brew: &impl Brew,
    git: &impl GitStore,
    cache: &Path,
    snaps: &Snapshots,
    inv: &Inventory,
    before: &[Classified],
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    let after = classify_installed(brew, git, cache, snaps, inv)?;
    let before_map: std::collections::HashMap<_, _> = before
        .iter()
        .map(|(n, a, _, _, _)| (n.as_str(), *a))
        .collect();
    let mut eligible = Vec::new();
    let mut soaking = Vec::new();
    let mut gone = Vec::new();
    for (name, action, inst, cut, head) in &after {
        if verbose {
            let did = match action {
                DesiredAction::InstallCutoff => "eligible after this update",
                DesiredAction::RefuseTooNew => "still soaking",
                DesiredAction::RefuseYanked => "gone at HEAD",
                DesiredAction::NoOpAlreadySoaked => "already at cutoff",
                DesiredAction::LeaveAheadOfSoak => "ahead of soak",
                DesiredAction::LeaveNewerThanCutoff => "newer than cutoff",
                DesiredAction::HoldUnordered => "versions not comparable",
                DesiredAction::LeaveAutoUpdates => "updates itself",
                DesiredAction::RefuseDeprecated => "deprecated at HEAD",
            };
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    name,
                    *action,
                    inst.as_ref(),
                    cut.as_ref(),
                    head.as_ref(),
                    did
                )
            )?;
        }
        match action {
            DesiredAction::InstallCutoff => {
                let was = before_map.get(name.as_str());
                if was.is_none() || matches!(was, Some(DesiredAction::RefuseTooNew)) {
                    eligible.push(name.clone());
                }
            }
            DesiredAction::RefuseTooNew => soaking.push(name.clone()),
            DesiredAction::RefuseYanked => gone.push(name.clone()),
            DesiredAction::NoOpAlreadySoaked
            | DesiredAction::LeaveAheadOfSoak
            | DesiredAction::LeaveNewerThanCutoff
            | DesiredAction::LeaveAutoUpdates
            | DesiredAction::RefuseDeprecated
            | DesiredAction::HoldUnordered => {}
        }
    }
    write_section(out, "==> Became eligible", &eligible)?;
    write_section(out, "==> Still soaking", &soaking)?;
    write_section(out, "==> Gone at HEAD", &gone)?;
    if eligible.is_empty() && soaking.is_empty() && gone.is_empty() {
        writeln!(out, "no installed packages changed soak status")?;
    }
    Ok(())
}

pub fn prefetch_installed(git: &impl GitStore, cache: &Path, inv: &Inventory, snaps: &Snapshots) {
    for pkg in snapshotted_pkgs(inv, snaps) {
        let _ = resolve_pkg_blobs(git, snaps, cache, &pkg.origin, &pkg.name, pkg.kind);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn update(
    lock: &RunLock,
    brew: &impl Brew,
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    cfg: &Config,
    inv: &Inventory,
    now: time::OffsetDateTime,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    writeln!(
        out,
        "updating soak snapshots; soak window {}h",
        cfg.hours.get()
    )?;
    let previous = snapshot::load_state(cache)?;
    let before = match &previous {
        Some(prev) => classify_installed(brew, git, cache, prev, inv).unwrap_or_default(),
        None => Vec::new(),
    };
    let plan = refresh_plan(cfg, inv, &[]);
    let snaps = snapshot::refresh_with(lock, git, gh, cache, &plan, now, out)?;
    prune::prune_cache(lock, cache, inv, verbose, out)?;
    writeln!(
        out,
        "core cutoff: {}",
        report::format_cutoff(&snaps.core.cutoff_sha, snaps.core.cutoff_time)
    )?;
    writeln!(
        out,
        "core head: {}",
        report::short_sha(&snaps.core.head_sha)
    )?;
    writeln!(
        out,
        "cask cutoff: {}",
        report::format_cutoff(&snaps.cask.cutoff_sha, snaps.cask.cutoff_time)
    )?;
    writeln!(
        out,
        "cask head: {}",
        report::short_sha(&snaps.cask.head_sha)
    )?;
    for (name, state) in &snaps.taps {
        match &state.cutoff_sha {
            Some(sha) => writeln!(
                out,
                "{name} cutoff: {}",
                report::format_cutoff(sha, state.cutoff_time)
            )?,
            None => writeln!(
                out,
                "{name} cutoff: none (no commit is older than the soak window)"
            )?,
        }
        writeln!(out, "{name} head: {}", report::short_sha(&state.head_sha))?;
    }
    for (name, err) in &snaps.held_taps {
        writeln!(
            out,
            "tap {name} could not be refreshed; its packages are held.\n{}",
            indented_detail(err)
        )?;
    }
    prefetch_installed(git, cache, inv, &snaps);
    write_update_summary(brew, git, cache, &snaps, inv, &before, verbose, out)?;
    if inv.any_no_soak() {
        writeln!(out, "no-soak packages installed; updating brew")?;
        let output = brew.run_visible(&["update".to_string()])?;
        write_session_tail(&brew.session_report(), out)?;
        if !output.status.success() {
            let status = output.status.code().unwrap_or(1);
            writeln!(out, "brew update failed (exit {status})")?;
            writeln!(out, "snapshots refreshed")?;
            return Err(Error::Brew {
                status,
                message: String::new(),
            });
        }
    }
    writeln!(out, "snapshots refreshed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::brew::MockBrew;

    use crate::git::InMemoryGit;
    use crate::github::{CommitInfo, StaticGithub};

    use crate::cmd::test_support::{
        cfg_with, cfg24, core_snaps, formula_pkg, formula_pkg_pinned, formula_rb, inv_from,
        lock_runs, many_detect_world, now, run_has_token, spawns,
    };
    use time::Duration;

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
    fn update_writes_shas_and_creates_state_toml() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&MockBrew::new(), &cfg);
        update(
            &RunLock::for_tests(),
            &MockBrew::new(),
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
            now(),
            false,
            &mut out,
        )
        .expect("update");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("thirtyh"), "{text}");
        assert!(text.contains("headsha"), "{text}");
        assert!(text.contains("fetching Homebrew/homebrew-core"), "{text}");
        assert!(text.contains("soak window 24h"), "{text}");
        assert!(
            dir.path().join("state.toml").is_file(),
            "state.toml missing"
        );
    }

    #[test]
    fn update_summary_prefetches_pinned_formulae_too() {
        let (mut brew, git) = many_detect_world(&["app", "bar", "baz"]);
        brew.installed[1] = formula_pkg_pinned("bar", formula_rb("bar", "1.0.0", "oldsha"));
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        classify_installed(&brew, &git, cache.path(), &core_snaps(), &inv).unwrap();
        assert_eq!(spawns(&brew), (1, 1));
    }

    #[test]
    fn update_runs_brew_update_once_only_when_a_no_soak_package_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", formula_rb("wget", "1.0.0", "a"))],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        update(
            &RunLock::for_tests(),
            &brew,
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
            now(),
            false,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !run_has_token(&lock_runs(&brew), "update"),
            "no no-soak package: no brew update"
        );
        let cfg = cfg_with("NO_SOAK = [\"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        update(
            &RunLock::for_tests(),
            &brew,
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
            now(),
            false,
            &mut out,
        )
        .unwrap();
        let updates = lock_runs(&brew)
            .iter()
            .filter(|a| a == &&vec!["update".to_string()])
            .count();
        assert_eq!(updates, 1);
        assert!(String::from_utf8(out).unwrap().contains("updating brew"));
    }

    #[test]
    fn update_reports_a_failed_brew_update_after_writing_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let git = InMemoryGit::new();
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", formula_rb("wget", "1.0.0", "a"))],
            next_status: 3,
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let err = update(
            &RunLock::for_tests(),
            &brew,
            &git,
            &fixture_gh(),
            dir.path(),
            &cfg,
            &inv,
            now(),
            false,
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Brew { status: 3, .. }), "{err:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("brew update failed (exit 3)"), "{text}");
        assert!(dir.path().join("state.toml").is_file());
    }
}
