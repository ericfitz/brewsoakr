use super::RunResult;
use super::apply::apply_many;
use super::bare::plan_size;
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::flags;
use crate::git::GitStore;
use crate::inventory::Inventory;
use crate::snapshot::Snapshots;
use std::io::Write;
use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub fn upgrade(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    inv: &Inventory,
    cfg: &Config,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    // A bare `brew soak upgrade` walks everything installed, so say up front
    // how many will actually change and count them off as they go.
    let plan_total = (names.is_empty())
        .then(|| plan_size(brew, git, snaps, cache, inv, flags::greedy_mode(user_flags)));
    apply_many(
        brew,
        git,
        snaps,
        cache,
        tap_root,
        inv,
        cfg,
        names,
        names.is_empty(),
        "upgrade",
        false,
        false,
        user_flags,
        plan_total,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::test_support::summary_has;

    use crate::brew::MockBrew;

    use crate::git::InMemoryGit;

    use crate::origin::OriginRecords;
    use crate::resolve::PkgKind;

    use std::path::Path;

    use crate::cmd::test_support::{
        cask_pkg_from, cask_rb, cfg_with, cfg24, core_snaps, formula_pkg, formula_pkg_from,
        formula_pkg_pinned, formula_rb, git_empty, inv_from, lock_runs, run_has_token,
        run_is_soaked_install, tap_cask_world, tap_snaps, tapped, unused_cache,
    };

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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "ok"),
            "expected soaked path install of ok without --ignore-dependencies: {runs:?}"
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
            text.contains(
                "\nNotes\n  Ahead of soak (installed artifact left unchanged):\n    ahead\n"
            ),
            "missing ahead entry: {text}"
        );
    }

    /// Installed at the cutoff while HEAD is newer: brew calls it outdated,
    /// so brewsoak must say a newer version is soaking instead of calling it
    /// plain "already soaked".
    fn soaking_upgrade(names: &[String]) -> String {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid.clone());
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_mid)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            names,
            &[],
            &mut out,
        )
        .expect("upgrade");
        assert!(lock_runs(&brew).is_empty(), "soaking must not invoke brew");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn named_upgrade_says_a_newer_version_is_soaking() {
        let text = soaking_upgrade(&["wget".to_string()]);
        assert!(summary_has(&text, "soaking", "1"), "{text}");
        assert!(summary_has(&text, "already soaked", "0"), "{text}");
        assert!(
            text.contains(
                "wget: 1.2.0 is still in the 24h soak window; installed 1.1.0 is the newest soaked version"
            ),
            "{text}"
        );
        assert!(!text.contains("already soaked: "), "{text}");
    }

    #[test]
    fn bare_upgrade_zero_plan_prints_header() {
        let text = soaking_upgrade(&[]);
        assert!(
            text.starts_with("Upgrading soaked packages (0 of 1)\n"),
            "{text}"
        );
    }

    #[test]
    fn named_run_nothing_to_do_no_soaked_header() {
        let text = soaking_upgrade(&["wget".to_string()]);
        assert!(!text.contains("soaked packages"), "{text}");
        assert!(text.starts_with("Summary\n"), "{text}");
    }

    #[test]
    fn bare_upgrade_names_soaking_packages_in_one_note() {
        let text = soaking_upgrade(&[]);
        assert!(summary_has(&text, "soaking", "1"), "{text}");
        assert!(
            text.contains("\nNotes\n  Still soaking (left at the cutoff):\n    wget 1.2.0\n"),
            "{text}"
        );
    }

    #[test]
    fn upgrade_already_soaked_is_silent_by_default() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid.clone());
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_mid)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("wget is already soaked"),
            "already soaked must be silent without -v: {text}"
        );
        assert!(
            text.contains("already soaked"),
            "summary must mention already soaked: {text}"
        );
        assert!(
            lock_runs(&brew).is_empty(),
            "already soaked must not invoke brew"
        );
    }

    fn deprecated_rb(name: &str, version: &str, sha: &str) -> String {
        format!(
            "{}\n  deprecate! date: \"2024-01-01\", because: :unmaintained\n",
            formula_rb(name, version, sha).trim_end()
        )
    }

    fn bare_upgrade(git: &InMemoryGit, brew: &MockBrew) -> (RunResult, String) {
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        let result = upgrade(
            brew,
            git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        (result, String::from_utf8(out).expect("utf8"))
    }

    #[test]
    fn upgrade_deprecated_but_current_warns_without_refusing() {
        let tcl = deprecated_rb("tcl", "8.6.18", "tclsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/t/tcl.rb", tcl.clone());
        git.insert_blob("headsha", "Formula/t/tcl.rb", tcl.clone());
        let brew = MockBrew {
            installed: vec![formula_pkg("tcl", tcl)],
            ..MockBrew::new()
        };
        let (result, text) = bare_upgrade(&git, &brew);
        assert!(!result.refused, "nothing to upgrade is not a refusal");
        assert!(!text.contains("bypass brewsoak"), "{text}");
        assert!(
            text.contains("warning: tcl is deprecated upstream; consider replacing it"),
            "{text}"
        );
        assert!(lock_runs(&brew).is_empty(), "must not invoke brew");
    }

    #[test]
    fn upgrade_deprecated_ahead_of_soak_warns_without_refusing() {
        let mid = formula_rb("tcl", "8.6.17", "midsha");
        let head = deprecated_rb("tcl", "8.6.18", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/t/tcl.rb", mid);
        git.insert_blob("headsha", "Formula/t/tcl.rb", head.clone());
        let brew = MockBrew {
            installed: vec![formula_pkg("tcl", head)],
            ..MockBrew::new()
        };
        let (result, text) = bare_upgrade(&git, &brew);
        assert!(!result.refused, "{text}");
        assert!(
            text.contains("Ahead of soak (installed artifact left unchanged):\n    tcl\n"),
            "{text}"
        );
        assert!(
            text.contains("warning: tcl is deprecated upstream"),
            "{text}"
        );
    }

    #[test]
    fn upgrade_deprecated_and_outdated_is_still_refused() {
        let old = formula_rb("tcl", "8.6.16", "oldsha");
        let mid = deprecated_rb("tcl", "8.6.17", "midsha");
        let head = deprecated_rb("tcl", "8.6.18", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/t/tcl.rb", mid);
        git.insert_blob("headsha", "Formula/t/tcl.rb", head);
        let brew = MockBrew {
            installed: vec![formula_pkg("tcl", old)],
            ..MockBrew::new()
        };
        let (result, text) = bare_upgrade(&git, &brew);
        assert!(result.refused, "{text}");
        assert!(
            text.contains(
                "tcl is deprecated or disabled at HEAD; use `brew upgrade tcl` to bypass brewsoak."
            ),
            "{text}"
        );
        assert!(
            !text.contains("consider replacing"),
            "the refusal already says why: {text}"
        );
        assert!(
            !lock_runs(&brew)
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("install")),
            "must not install a deprecated package"
        );
    }

    #[test]
    fn upgrade_already_soaked_prints_when_verbose() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid.clone());
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_mid)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &["--verbose".to_string()],
            &mut out,
        )
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("wget:") && text.contains("soaked"),
            "verbose upgrade must print already soaked: {text}"
        );
    }

    #[test]
    fn upgrade_nameless_skips_unparseable_and_continues() {
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let ok_new = formula_rb("ok", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob("headsha", "Formula/o/ok.rb", ok_new);
        git.insert_blob("cutoffsha", "Formula/b/bad.rb", "class Bad; end\n");
        git.insert_blob("headsha", "Formula/b/bad.rb", "class Bad; end\n");

        let brew = MockBrew {
            installed: vec![
                formula_pkg("bad", "class Bad; end\n".into()),
                formula_pkg("ok", ok_old),
            ],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "ok"),
            "parse failure must not abort remaining upgrades: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/bad"),
            "unparseable must not install: {runs:?}"
        );
        assert!(!result.refused, "unparseable skip is not a soak refusal");
    }

    #[test]
    fn upgrade_nameless_skips_pinned_formula() {
        let pin_old = formula_rb("pin", "1.0.0", "oldsha");
        let pin_mid = formula_rb("pin", "1.1.0", "midsha");
        let pin_new = formula_rb("pin", "1.2.0", "newsha");
        let ok_old = formula_rb("ok", "1.0.0", "oldsha");
        let ok_mid = formula_rb("ok", "1.1.0", "midsha");
        let ok_new = formula_rb("ok", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/p/pin.rb", pin_mid);
        git.insert_blob("headsha", "Formula/p/pin.rb", pin_new);
        git.insert_blob("cutoffsha", "Formula/o/ok.rb", ok_mid);
        git.insert_blob("headsha", "Formula/o/ok.rb", ok_new);

        let brew = MockBrew {
            installed: vec![
                formula_pkg_pinned("pin", pin_old),
                formula_pkg("ok", ok_old),
            ],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "ok"),
            "unpinned must still upgrade: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/pin"),
            "pinned must be skipped: {runs:?}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn upgrade_bare_soaks_tap_package_from_its_tap_snapshot() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "hashicorp/tap",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("upgrading terraform 1.0.0 -> 1.1.0"),
            "{text}"
        );
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "terraform"),
            "{:?}",
            lock_runs(&brew)
        );
        assert!(!r.refused);
        assert!(
            !run_has_token(&lock_runs(&brew), "hashicorp/tap/terraform"),
            "soaked tap packages are never brew tokens"
        );
    }

    #[test]
    fn staged_tap_upgrade_records_origin_despite_outdated_notice() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "hashicorp/tap",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            // What brew prints on every upgrade of an outdated keg.
            next_stdout: b"Warning: terraform 1.0.0 is already installed but outdated (so it will be upgraded).\n"
                .to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"),
            Some("hashicorp/tap")
        );
    }

    #[test]
    fn upgrade_bare_soaks_tap_cask_from_its_tap_snapshot() {
        let (brew, git, snaps) = tap_cask_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("ant: origin anthropics/tap; soak 24h"),
            "{text}"
        );
        assert!(text.contains("upgrading ant 1.37.0 -> 1.38.0"), "{text}");
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "ant"), "{runs:?}");
        assert!(
            runs.iter().any(|a| a.iter().any(|x| x == "--cask")
                && a.iter().any(|x| x.ends_with("/Casks/ant.rb"))),
            "staged from the tap's Casks/ path: {runs:?}"
        );
        assert!(!r.refused);
    }

    /// Installed core formula `vacuum` and tap cask `daveshanley/vacuum/vacuum`,
    /// both behind their cutoffs.
    fn same_name_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/v/vacuum.rb",
            formula_rb("vacuum", "0.30.6", "s"),
        );
        git.insert_blob(
            "headsha",
            "Formula/v/vacuum.rb",
            formula_rb("vacuum", "0.30.6", "s"),
        );
        git.insert_blob("tapcut", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0"));
        git.insert_blob("taphead", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0"));
        let brew = MockBrew {
            installed: vec![
                formula_pkg_from(
                    "vacuum",
                    "homebrew/core",
                    formula_rb("vacuum", "0.30.0", "old"),
                ),
                cask_pkg_from(
                    "vacuum",
                    "daveshanley/vacuum",
                    crate::brew::version_only_cask_receipt("vacuum", "0.30.0"),
                ),
            ],
            taps: vec![tapped(
                "daveshanley/vacuum",
                Some("https://github.com/daveshanley/homebrew-vacuum"),
            )],
            ..MockBrew::new()
        };
        let snaps = tap_snaps(&git, "daveshanley/vacuum", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Casks/vacuum.rb"]);
        git.insert_tree("taphead", &["Casks/vacuum.rb"]);
        (brew, git, snaps)
    }

    #[test]
    fn upgrade_bare_evaluates_same_name_formula_and_tap_cask_once_each() {
        let (brew, git, snaps) = same_name_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("upgrading vacuum 0.30.0 -> 0.30.6"),
            "formula: {text}"
        );
        assert!(
            text.contains("upgrading vacuum 0.30.0 -> 0.31.0"),
            "cask: {text}"
        );
        assert_eq!(text.matches("upgrading vacuum").count(), 2, "{text}");
        let runs = lock_runs(&brew);
        let installs: Vec<&Vec<String>> = runs
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .collect();
        assert_eq!(installs.len(), 2, "{runs:?}");
        assert!(
            installs.iter().any(|a| a.iter().any(|x| x == "--cask")
                && a.iter().any(|x| x.ends_with("/Casks/vacuum.rb"))),
            "cask staged from the tap's Casks/ path: {runs:?}"
        );
        assert!(
            installs
                .iter()
                .any(|a| !a.iter().any(|x| x == "--cask")
                    && a.iter().any(|x| x.ends_with("/vacuum.rb"))),
            "formula staged: {runs:?}"
        );
    }

    #[test]
    fn a_formula_poured_earlier_does_not_mark_a_same_name_cask_done() {
        let (mut brew, git, snaps) = same_name_world();
        // brew pours formula `vacuum` at the cask's cutoff version.
        brew.next_stdout = "\u{1f37a}  /opt/homebrew/Cellar/vacuum/0.31.0: 5 files, 1MB\n"
            .as_bytes()
            .to_vec();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            lock_runs(&brew)
                .iter()
                .any(|a| a.iter().any(|x| x == "--cask")),
            "the cask must still be installed"
        );
    }

    #[test]
    fn upgrade_bare_header_total_equals_announced_steps() {
        let git = InMemoryGit::new();
        for name in ["left", "right"] {
            let dir = &name[..1];
            git.insert_blob(
                "cutoffsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.1.0", "midsha"),
            );
            git.insert_blob(
                "headsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.1.0", "midsha"),
            );
        }
        let auto = format!("{}  auto_updates true\n", cask_rb("alt-tab", "11.8.0"));
        git.insert_blob("caskcut", "Casks/a/alt-tab.rb", auto.clone());
        git.insert_blob("caskhead", "Casks/a/alt-tab.rb", auto);
        let (vbrew, vgit, snaps) = same_name_world();
        for (sha, path, body) in [
            (
                "cutoffsha",
                "Formula/v/vacuum.rb",
                formula_rb("vacuum", "0.30.6", "s"),
            ),
            (
                "headsha",
                "Formula/v/vacuum.rb",
                formula_rb("vacuum", "0.30.6", "s"),
            ),
            ("tapcut", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0")),
            ("taphead", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0")),
        ] {
            git.insert_blob(sha, path, body);
        }
        drop(vgit);
        git.insert_tree("tapcut", &["Casks/vacuum.rb"]);
        git.insert_tree("taphead", &["Casks/vacuum.rb"]);
        let mut installed = vec![
            formula_pkg("left", formula_rb("left", "1.0.0", "oldsha")),
            formula_pkg("right", formula_rb("right", "1.0.0", "oldsha")),
            cask_pkg_from(
                "alt-tab",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("alt-tab", "7.38.1"),
            ),
        ];
        installed.extend(vbrew.installed.clone());
        // brew pours `right` while installing `left`: it is already at cutoff
        // when its turn comes.
        let brew = MockBrew {
            installed,
            taps: vbrew.taps.clone(),
            next_stdout: "\u{1f37a}  /opt/homebrew/Cellar/right/1.1.0: 5 files, 1MB\n"
                .as_bytes()
                .to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let header = text
            .lines()
            .find_map(|l| {
                l.strip_prefix("Upgrading soaked packages (")?
                    .split(' ')
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
            .expect("header");
        let announced = text
            .lines()
            .filter(|l| l.trim_start().starts_with('[') && l.contains(&format!("/{header}] ")))
            .count();
        assert_eq!(header, 4, "{text}");
        assert_eq!(announced, header, "{text}");
        assert!(!text.contains("alt-tab 7.38.1 ->"), "{text}");
    }

    #[test]
    fn tap_without_cutoff_holds_its_packages_as_too_new() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "hashicorp/tap",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", None, "taphead");
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert!(text.contains("terraform is too new"), "{text}");
        assert!(
            lock_runs(&brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install")),
            "nothing installed"
        );
    }

    #[test]
    fn held_tap_holds_only_its_packages_with_git_note() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/a/alpha.rb",
            formula_rb("alpha", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/a/alpha.rb",
            formula_rb("alpha", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![
                formula_pkg("alpha", formula_rb("alpha", "1.0.0", "oldsha")),
                formula_pkg_from(
                    "terraform",
                    "hashicorp/tap",
                    formula_rb("terraform", "1.0.0", "oldsha"),
                ),
            ],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = core_snaps();
        snaps.held_taps.insert(
            "hashicorp/tap".into(),
            "while fetching history from origin, git failed:\nfatal: boom".into(),
        );
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "alpha"),
            "core package still upgraded"
        );
        assert!(r.refused);
        assert!(
            text.contains("terraform: tap hashicorp/tap could not be refreshed")
                && text.contains("fatal: boom"),
            "{text}"
        );
    }

    #[test]
    fn unsoakable_tap_package_is_noted_in_bare_upgrade_and_refused_when_named() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "thing",
                "local/tap",
                formula_rb("thing", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped("local/tap", None)],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = core_snaps();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git_empty(),
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!r.refused, "bare upgrade only notes unsoakable packages");
        assert!(text.contains("thing: tap local/tap has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK"), "{text}");
        let mut out = Vec::new();
        let r = upgrade(
            &brew,
            &git_empty(),
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["local/tap/thing".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused, "explicit unsoakable is a refusal");
        assert!(
            text.contains("use `brew upgrade local/tap/thing`"),
            "{text}"
        );
        assert!(lock_runs(&brew).is_empty());
    }

    #[test]
    fn upgrade_summary_golden() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/a/alpha.rb",
            formula_rb("alpha", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/a/alpha.rb",
            formula_rb("alpha", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
            installed: vec![
                formula_pkg("alpha", formula_rb("alpha", "1.0.0", "oldsha")),
                formula_pkg_from(
                    "brewsoak",
                    "ericfitz/tap",
                    formula_rb("brewsoak", "1.0.0", "b"),
                ),
                formula_pkg_from("thing", "local/tap", formula_rb("thing", "1.0.0", "t")),
                formula_pkg_from(
                    "terraform",
                    "hashicorp/tap",
                    formula_rb("terraform", "1.0.0", "t"),
                ),
                formula_pkg_pinned("curl", formula_rb("curl", "1.0.0", "c")),
            ],
            taps: vec![
                tapped(
                    "ericfitz/tap",
                    Some("https://github.com/ericfitz/homebrew-tap"),
                ),
                tapped("local/tap", None),
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
            ],
            next_stdout: b"\xf0\x9f\x8d\xba  /opt/homebrew/Cellar/alpha/1.1.0: 5 files, 1MB\n"
                .to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut snaps = core_snaps();
        snaps.held_taps.insert(
            "hashicorp/tap".into(),
            "while fetching history from origin to find the tap's soak cutoff, git failed:\nfatal: unable to access".into(),
        );
        let tap = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let got = String::from_utf8(out).unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upgrade_summary.txt");
        if std::env::var_os("BREWSOAK_BLESS").is_some() {
            std::fs::write(&path, &got).unwrap();
        }
        let want = std::fs::read_to_string(&path)
            .expect("fixture missing; run with BREWSOAK_BLESS=1 to create it");
        assert_eq!(
            got, want,
            "upgrade summary changed; rerun with BREWSOAK_BLESS=1 if intended"
        );
        assert!(summary_has(&got, "no-soak", "1"), "{got}");
        assert!(got.contains("\nNotes\n"), "{got}");
        assert!(
            got.contains("thing: tap local/tap has no HTTPS remote"),
            "{got}"
        );
        assert!(
            got.contains("terraform: tap hashicorp/tap could not be refreshed"),
            "{got}"
        );
    }
}
