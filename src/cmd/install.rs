use super::RunResult;
use super::apply::apply_many;
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::git::GitStore;
use crate::inventory::Inventory;
use crate::snapshot::Snapshots;
use std::io::Write;
use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub fn install(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    tap_root: &Path,
    inv: &Inventory,
    cfg: &Config,
    names: &[String],
    force_cask: bool,
    force_formula: bool,
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    if names.is_empty() {
        return Err(Error::Usage("install: no packages specified".into()));
    }
    apply_many(
        brew,
        git,
        snaps,
        cache,
        tap_root,
        inv,
        cfg,
        names,
        false,
        "install",
        force_cask,
        force_formula,
        user_flags,
        None,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::brew::MockBrew;

    use crate::git::InMemoryGit;

    use crate::inventory::Inventory;
    use crate::origin::OriginRecords;
    use crate::resolve::PkgKind;

    use crate::cmd::RunResult;
    use crate::cmd::apply::reinstall;

    use crate::cmd::test_support::{
        PACKER_REFUSAL, call_reinstall, cask_pkg_from, cask_rb, cfg_with, cfg24, core_snaps,
        formula_pkg, formula_pkg_from, formula_rb, git_empty, hashicorp_tap, inv_from, lock_runs,
        packer_world, run_has_token, run_is_soaked_install, tap_snaps, tapped, unused_cache,
    };

    #[test]
    fn soaked_tap_install_stages_under_tap_dir_and_writes_origin_record() {
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
        install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let staged = tap.path().join("taps/hashicorp/tap/Formula/terraform.rb");
        assert!(staged.exists(), "staged under the per-tap directory");
        assert!(
            !tap.path().join("Formula/terraform.rb").exists(),
            "never under the core staging root"
        );
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|a| a.iter().any(|x| x == staged.to_str().unwrap())),
            "{runs:?}"
        );
        let records = OriginRecords::load(cache.path());
        assert_eq!(
            records.get(PkgKind::Formula, "terraform"),
            Some("hashicorp/tap")
        );
    }

    #[test]
    fn dry_run_tap_install_writes_no_origin_record_and_says_would_upgrade() {
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
        install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &["--dry-run".to_string()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!cache.path().join("origins.toml").exists());
        assert!(text.contains("would upgrade 1,"), "{text}");
        assert!(!text.contains("upgraded 1"), "{text}");
    }

    #[test]
    fn dry_run_core_install_keeps_a_stale_origin_record() {
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
        let cache = tempfile::tempdir().unwrap();
        let mut records = OriginRecords::default();
        records.set(PkgKind::Formula, "alpha", "old/tap");
        records.save(cache.path()).unwrap();
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        install(
            &brew,
            &git,
            &core_snaps(),
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["alpha".into()],
            false,
            false,
            &["-n".to_string()],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "alpha"),
            Some("old/tap")
        );
    }

    #[test]
    fn core_install_removes_a_stale_origin_record() {
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
        let cache = tempfile::tempdir().unwrap();
        let mut records = OriginRecords::default();
        records.set(PkgKind::Formula, "alpha", "old/tap");
        records.save(cache.path()).unwrap();
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        install(
            &brew,
            &git,
            &core_snaps(),
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["alpha".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "alpha"),
            None
        );
    }

    #[test]
    fn staged_load_failure_holds_with_note_and_does_not_fail_the_run() {
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
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            next_status: 1,
            next_stdout: b"Error: cannot load such file -- ../lib/helper\n".to_vec(),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert_eq!(
            r.brew_status, None,
            "a staged-load hold is not a brew failure"
        );
        assert!(
            text.contains(
                "terraform: cannot be installed from a staged copy; use brew, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(text.contains("held 1"), "{text}");
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"),
            None
        );
    }

    fn install_packer(
        brew: &MockBrew,
        git: &InMemoryGit,
        snaps: &Snapshots,
    ) -> (RunResult, String) {
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        let cache = tempfile::tempdir().unwrap();
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            brew,
            git,
            snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/packer".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn untrusted_tap_refusal_holds_the_target_and_never_runs_brew_trust() {
        let (git, snaps) = packer_world();
        let brew = MockBrew {
            taps: hashicorp_tap(),
            next_status: 1,
            next_stdout: PACKER_REFUSAL.to_vec(),
            ..MockBrew::new()
        };
        let (r, text) = install_packer(&brew, &git, &snaps);
        assert!(r.refused);
        assert_eq!(r.brew_status, None, "a trust hold is not a brew failure");
        assert!(
            text.contains(
                "packer: brew does not trust tap hashicorp/tap; run brew trust hashicorp/tap, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(
            text.contains("upgraded 0,") && text.contains("held 1"),
            "{text}"
        );
        let all: Vec<Vec<String>> = lock_runs(&brew)
            .into_iter()
            .chain(brew.visible_runs.lock().unwrap().clone())
            .collect();
        assert!(
            !all.iter().any(|a| a.iter().any(|x| x == "trust")),
            "brewsoak must never trust a third-party tap: {all:?}"
        );
    }

    #[test]
    fn failed_brew_run_is_not_counted_as_upgraded() {
        let (git, snaps) = packer_world();
        let brew = MockBrew {
            taps: hashicorp_tap(),
            next_status: 1,
            next_stdout: b"Error: packer: download failed\n".to_vec(),
            ..MockBrew::new()
        };
        let (r, text) = install_packer(&brew, &git, &snaps);
        assert_eq!(r.brew_status, Some(1), "{text}");
        assert!(text.contains("upgraded 0,"), "{text}");
        assert!(!text.contains("held 1"), "a failure is not a hold: {text}");
    }

    #[test]
    fn failed_reinstall_repair_is_not_counted_as_upgraded() {
        let wget_new = formula_rb("wget", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new.clone());
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_new)],
            next_status: 1,
            next_stdout: b"Error: wget: download failed\n".to_vec(),
            ..MockBrew::new()
        };
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let result = call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["wget".to_string()],
            &mut out,
        )
        .expect("reinstall");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(result.brew_status, Some(1), "{text}");
        assert!(text.contains("upgraded 0,"), "{text}");
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
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
    fn reinstall_true_repair() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid);
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new.clone());

        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_new)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["wget".to_string()];
        let result =
            call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out).expect("reinstall");
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|args| args == &["reinstall".to_string(), "wget".to_string()]),
            "true repair must brew reinstall wget: {runs:?}"
        );
        assert!(
            !runs
                .iter()
                .any(|args| args.iter().any(|a| a.contains("brewsoakr/soaked"))),
            "true repair must not use soaked tap: {runs:?}"
        );
        assert!(!result.refused, "true repair is not a refusal");
    }

    #[test]
    fn reinstall_cask_true_repair_with_version_only_receipt() {
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/a/app.rb", cask_rb("app", "1.0.0"));
        git.insert_blob("caskhead", "Casks/a/app.rb", cask_rb("app", "1.1.0"));
        let brew = MockBrew {
            installed: vec![cask_pkg_from(
                "app",
                "homebrew/cask",
                crate::brew::version_only_cask_receipt("app", "1.1.0"),
            )],
            ..MockBrew::new()
        };
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["app".to_string()],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|a| a == &["reinstall".to_string(), "app".to_string()]),
            "installed == HEAD is a true repair: {runs:?}"
        );
    }

    #[test]
    fn reinstall_true_repair_when_receipt_has_no_rebuild() {
        let head = |rebuild: bool| {
            let bottle = if rebuild {
                "  bottle do\n    rebuild 1\n    sha256 cellar: :any, arm64_tahoe: \"bbb\"\n  end\n"
            } else {
                ""
            };
            format!(
                "class Wget < Formula\n  url \"https://example.com/wget-1.2.0.tar.gz\"\n  sha256 \"newsha\"\n{bottle}end\n"
            )
        };
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob("headsha", "Formula/w/wget.rb", head(true));
        let brew = MockBrew {
            installed: vec![formula_pkg("wget", head(false))],
            ..MockBrew::new()
        };
        let tap = tempfile::tempdir().unwrap();
        call_reinstall(
            &brew,
            &git,
            &core_snaps(),
            tap.path(),
            &["wget".to_string()],
            &mut Vec::new(),
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert!(
            runs.iter()
                .any(|a| a == &["reinstall".to_string(), "wget".to_string()]),
            "a receipt without rebuild still matches HEAD: {runs:?}"
        );
    }

    #[test]
    fn malformed_explicit_token_is_rejected_before_any_brew_run() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "cutoffsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "headsha",
            "Formula/w/wget.rb",
            formula_rb("wget", "1.1.0", "midsha"),
        );
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let names = ["wget".to_string(), "a/b".to_string()];
        let mut out = Vec::new();
        let err = install(
            &brew,
            &git,
            &core_snaps(),
            tempfile::tempdir().unwrap().path(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(
            lock_runs(&brew).is_empty(),
            "no side effects: {:?}",
            lock_runs(&brew)
        );
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn reinstall_already_soaked() {
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
        let names = ["wget".to_string()];
        let result =
            call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out).expect("reinstall");
        let runs = lock_runs(&brew);
        assert!(
            runs.is_empty(),
            "already soaked must not brew reinstall or install: {runs:?}"
        );
        assert!(!result.refused, "already soaked is not a refusal");
    }

    #[test]
    fn reinstall_behind() {
        let wget_old = formula_rb("wget", "1.0.0", "oldsha");
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid);
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);

        let brew = MockBrew {
            installed: vec![formula_pkg("wget", wget_old)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["wget".to_string()];
        let result =
            call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out).expect("reinstall");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_install(&runs, "wget"),
            "behind reinstall must path-install cutoff without --ignore-dependencies: {runs:?}"
        );
        assert!(
            !runs
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("reinstall")),
            "behind reinstall must not brew reinstall HEAD: {runs:?}"
        );
        assert!(!result.refused, "eligible behind reinstall must not refuse");
    }

    #[test]
    fn reinstall_missing() {
        let wget_mid = formula_rb("wget", "1.1.0", "midsha");
        let wget_new = formula_rb("wget", "1.2.0", "newsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", wget_mid);
        git.insert_blob("headsha", "Formula/w/wget.rb", wget_new);

        let brew = MockBrew::new();
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["wget".to_string()];
        let err = call_reinstall(&brew, &git, &snaps, tap.path(), &names, &mut out)
            .expect_err("missing reinstall must refuse");
        let runs = lock_runs(&brew);
        assert!(
            runs.is_empty(),
            "missing reinstall must not run brew: {runs:?}"
        );
        match err {
            Error::Refusal(msg) => {
                assert!(
                    msg.contains("no installed keg"),
                    "missing reinstall refusal: {msg}"
                );
            }
            other => panic!("expected Error::Refusal, got {other:?}"),
        }
    }

    #[test]
    fn install_uses_file_path_not_tap_token() {
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            runs.iter().any(|args| {
                args.first().map(String::as_str) == Some("install")
                    && args.iter().any(|a| a.ends_with("/fresh.rb"))
                    && !args.iter().any(|a| a.contains("brewsoakr/soaked"))
            }),
            "install must use a file path, not a tap token: {runs:?}"
        );
    }

    #[test]
    fn install_uses_run_visible_for_soaked_install() {
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
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let visible = brew.visible_runs.lock().expect("visible");
        assert!(
            visible
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("install")
                    && args.iter().any(|a| a.ends_with("/fresh.rb"))),
            "soaked install must use run_visible: {visible:?}"
        );
    }

    #[test]
    fn install_already_installed_nonzero_is_success() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        let brew = MockBrew {
            next_status: 1,
            next_stderr: b"Error: fresh 1.1.0 is already installed\n".to_vec(),
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            false,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        assert!(
            !result.refused,
            "already-installed must not be a soak refusal"
        );
        assert_eq!(
            result.brew_status,
            Some(0),
            "non-zero already-installed must be treated as success"
        );
    }

    #[test]
    fn upgrade_explicit_tap_token_is_soaked_not_passed_through() {
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
        install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "terraform"), "{runs:?}");
        assert!(!run_has_token(&runs, "hashicorp/tap/terraform"), "{runs:?}");
    }

    #[test]
    fn explicit_token_for_untapped_tap_is_refused_with_tap_hint() {
        let brew = MockBrew::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["nobody/tap/x".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("tap nobody/tap is not installed; run brew tap nobody/tap first, or add it to NO_SOAK"), "{text}");
        assert!(lock_runs(&brew).is_empty());
        // No-soak entry: brew handles it (brew auto-taps).
        let cfg = cfg_with("NO_SOAK = [\"nobody/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["nobody/tap/x".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        assert_eq!(runs[0], vec!["update".to_string()]);
        assert_eq!(runs[1], vec!["install", "nobody/tap/x"]);
    }

    #[test]
    fn reinstall_no_soak_goes_to_brew_reinstall_after_update() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "brewsoak",
                "ericfitz/tap",
                formula_rb("brewsoak", "1.0.0", "b"),
            )],
            taps: vec![tapped(
                "ericfitz/tap",
                Some("https://github.com/ericfitz/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let r = reinstall(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["brewsoak".into()],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        assert_eq!(
            runs,
            vec![
                vec!["update".to_string()],
                vec!["reinstall".to_string(), "ericfitz/tap/brewsoak".to_string()]
            ]
        );
    }

    #[test]
    fn reinstall_true_repair_of_tap_package_uses_full_token() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
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
                formula_rb("terraform", "1.2.0", "newsha"),
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
        let tap = tempfile::tempdir().unwrap();
        reinstall(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["terraform".into()],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            lock_runs(&brew),
            vec![vec![
                "reinstall".to_string(),
                "hashicorp/tap/terraform".to_string()
            ]]
        );
    }

    fn repair_world(next_status: i32, next_stdout: &[u8]) -> (MockBrew, InMemoryGit, Snapshots) {
        let git = InMemoryGit::new();
        let rb = formula_rb("terraform", "1.2.0", "newsha");
        git.insert_blob("tapcut", "Formula/terraform.rb", rb.clone());
        git.insert_blob("taphead", "Formula/terraform.rb", rb.clone());
        let brew = MockBrew {
            installed: vec![formula_pkg_from("terraform", "hashicorp/tap", rb)],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            next_status,
            next_stdout: next_stdout.to_vec(),
            ..MockBrew::new()
        };
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        (brew, git, snaps)
    }

    /// Runs `reinstall terraform` over a cache holding `record`; returns the
    /// record afterwards.
    fn repair_record_after(
        next_status: i32,
        next_stdout: &[u8],
        record: Option<&str>,
    ) -> Option<String> {
        let (brew, git, snaps) = repair_world(next_status, next_stdout);
        let cache = tempfile::tempdir().unwrap();
        let mut origins = OriginRecords::default();
        if let Some(tap) = record {
            origins.set(PkgKind::Formula, "terraform", tap);
            origins.save(cache.path()).unwrap();
        }
        let cfg = cfg24();
        let inv = Inventory::build(brew.installed.clone(), &brew.taps, &origins, &cfg);
        let tap = tempfile::tempdir().unwrap();
        reinstall(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["terraform".into()],
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        OriginRecords::load(cache.path())
            .get(PkgKind::Formula, "terraform")
            .map(str::to_string)
    }

    #[test]
    fn reinstall_of_a_held_tap_package_is_held() {
        let (brew, git, mut snaps) = repair_world(0, b"");
        snaps.held_taps.insert(
            "hashicorp/tap".into(),
            "while fetching history from origin, git failed:\nfatal: boom".into(),
        );
        let cache = tempfile::tempdir().unwrap();
        let mut origins = OriginRecords::default();
        origins.set(PkgKind::Formula, "terraform", "hashicorp/tap");
        origins.save(cache.path()).unwrap();
        let cfg = cfg24();
        let inv = Inventory::build(brew.installed.clone(), &brew.taps, &origins, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = reinstall(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["terraform".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused, "{text}");
        assert!(
            lock_runs(&brew).iter().all(|a| a[0] != "reinstall"),
            "{:?}",
            lock_runs(&brew)
        );
        assert!(
            text.contains("terraform: tap hashicorp/tap could not be refreshed")
                && text.contains("fatal: boom"),
            "{text}"
        );
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"),
            Some("hashicorp/tap")
        );
    }

    #[test]
    fn repair_reinstall_removes_the_stale_origin_record() {
        let after = repair_record_after(
            0,
            b"\xf0\x9f\x8d\xba  /opt/homebrew/Cellar/terraform/1.2.0\n",
            Some("hashicorp/tap"),
        );
        assert_eq!(after, None);
    }

    #[test]
    fn failed_repair_reinstall_keeps_the_origin_record() {
        let after = repair_record_after(
            1,
            b"Error: terraform: download failed\n",
            Some("hashicorp/tap"),
        );
        assert_eq!(after.as_deref(), Some("hashicorp/tap"));
    }

    #[test]
    fn repair_reinstall_without_a_record_is_a_quiet_no_op() {
        let after = repair_record_after(0, b"", None);
        assert_eq!(after, None);
    }

    #[test]
    fn explicit_tap_token_over_a_core_keg_is_refused_not_switched() {
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
                "homebrew/core",
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
        let tap = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        assert!(String::from_utf8(out).unwrap().contains(
            "terraform is installed from homebrew/core; brewsoak does not switch taps; use brew"
        ));
        assert!(lock_runs(&brew).is_empty());
        assert_eq!(
            OriginRecords::load(cache.path()).get(PkgKind::Formula, "terraform"),
            None
        );
    }

    #[test]
    fn explicit_no_soak_tap_token_over_a_core_keg_is_also_refused() {
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "terraform",
                "homebrew/core",
                formula_rb("terraform", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"hashicorp/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git_empty(),
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("use brew install hashicorp/tap/terraform"),
            "{text}"
        );
        assert!(lock_runs(&brew).is_empty());
    }

    #[test]
    fn fresh_cask_install_honors_no_soak_for_homebrew_cask() {
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
        git.insert_blob("caskhead", "Casks/f/firefox.rb", cask_rb("firefox", "1.0"));
        let brew = MockBrew::new();
        let cfg = cfg_with("NO_SOAK = [\"homebrew/cask\"]\n");
        let inv = inv_from(&brew, &cfg);
        let tap = tempfile::tempdir().unwrap();
        install(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["firefox".into()],
            false,
            false,
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        let runs = lock_runs(&brew);
        assert_eq!(
            runs,
            vec![
                vec!["update".to_string()],
                vec!["install".to_string(), "firefox".to_string()]
            ],
            "{runs:?}"
        );
    }

    #[test]
    fn tap_package_refusal_hint_uses_full_token() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "taphead",
            "Formula/terraform.rb",
            formula_rb("terraform", "1.2.0", "newsha"),
        );
        let brew = MockBrew {
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
        install(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &["hashicorp/tap/terraform".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("use `brew install hashicorp/tap/terraform` to bypass")
        );
    }
}
