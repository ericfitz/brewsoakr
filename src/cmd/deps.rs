use super::apply::staging_dir;
use super::view::{
    cutoff_blob_exists, natural_kind, resolve_pkg_blobs, resolve_view, write_cutoff_blob,
};
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::eligibility::DesiredAction;
use crate::git::GitStore;
use crate::inventory::{self, Inventory, Pkg, PkgClass};
use crate::origin::{self};
use crate::resolve::PkgKind;
use crate::snapshot::Snapshots;
use crate::tap;
use std::collections::HashSet;
use std::path::Path;

/// A dependency in a tap that could not be refreshed this run.
pub(super) struct HeldDep {
    pub(super) dep: String,
    /// Why the dep's tap cannot be used, e.g. `tap acme/tools could not be
    /// refreshed`; git detail follows on its own lines.
    pub(super) reason: String,
}

pub(super) struct CutoffDeps {
    /// (origin, name, kind), dependencies first.
    pub(super) deps: Vec<(String, String, PkgKind)>,
    pub(super) held: Option<HeldDep>,
}

pub(super) struct CutoffDepWalk<'a, B, G> {
    pub(super) brew: &'a B,
    pub(super) git: &'a G,
    pub(super) snaps: &'a Snapshots,
    pub(super) cache: &'a Path,
    pub(super) tap_root: &'a Path,
    pub(super) inv: &'a Inventory,
    pub(super) cfg: &'a Config,
    pub(super) visiting: HashSet<String>,
    pub(super) visited: HashSet<String>,
    pub(super) out: Vec<(String, String, PkgKind)>,
    pub(super) held: Option<HeldDep>,
}

impl<B: Brew, G: GitStore> CutoffDepWalk<'_, B, G> {
    pub(super) fn visit(
        &mut self,
        origin_tap: &str,
        name: &str,
        kind: PkgKind,
        include_self: bool,
    ) -> Result<(), Error> {
        let key = format!("{origin_tap}/{name}");
        if self.visiting.contains(&key) || self.visited.contains(&key) {
            return Ok(());
        }
        self.visiting.insert(key.clone());
        if include_self {
            write_cutoff_blob(
                self.git,
                self.snaps,
                self.cache,
                self.tap_root,
                origin_tap,
                name,
                kind,
            )?;
        }
        let root = staging_dir(self.tap_root, origin_tap);
        let staged = match kind {
            PkgKind::Formula => tap::tap_formula_path(&root, name),
            PkgKind::Cask => tap::tap_cask_path(&root, name),
        };
        let token = staged.to_string_lossy().into_owned();
        for dep in self.brew.deps(kind, &token)? {
            // Already installed, from any origin: brew's, unless this run is
            // about to upgrade it; then it goes first, so the dependent never
            // links against a library the dependency has not yet shipped.
            let installed_name = inventory::parse_token(&dep)
                .map(|t| t.name)
                .unwrap_or_else(|_| dep.clone());
            if let Some(pkg) = self.inv.find(&installed_name) {
                if self.upgrades_this_run(pkg)? {
                    let (o, k) = (pkg.origin.clone(), pkg.kind);
                    self.visit(&o, &installed_name, k, true)?;
                    if self.held.is_some() {
                        return Ok(());
                    }
                }
                continue;
            }
            let Some((dep_origin, dep_name)) = self.dep_origin(&dep)? else {
                continue;
            };
            if self.inv.class_for(&dep_origin, &dep_name, self.cfg) != PkgClass::Soaked {
                continue; // no-soak or unsoakable deps are brew's
            }
            if let Some(err) = self.snaps.held_taps.get(&dep_origin) {
                // Its cutoff is unknown: refuse before staging anything.
                self.held = Some(HeldDep {
                    dep: dep_name,
                    reason: format!(
                        "tap {dep_origin} could not be refreshed;\n  {}",
                        err.replace('\n', "\n  ")
                    ),
                });
                return Ok(());
            }
            if !origin::is_core_or_cask(&dep_origin) && self.snaps.tap(&dep_origin).is_none() {
                self.held = Some(HeldDep {
                    dep: dep_name,
                    reason: format!("tap {dep_origin} has no soak snapshot this run"),
                });
                return Ok(());
            }
            let dep_kind = natural_kind(
                self.git,
                self.snaps,
                self.cache,
                self.inv,
                &dep_origin,
                &dep_name,
            )?;
            if !cutoff_blob_exists(
                self.git,
                self.snaps,
                self.cache,
                &dep_origin,
                &dep_name,
                dep_kind,
            )? {
                // Nothing to stage: install_missing_dep refuses the target
                // from the dep's upstream status (too new, not found).
                self.out.push((dep_origin, dep_name, dep_kind));
                continue;
            }
            self.visit(&dep_origin, &dep_name, dep_kind, true)?;
            if self.held.is_some() {
                return Ok(());
            }
        }
        self.visiting.remove(&key);
        self.visited.insert(key);
        if include_self {
            self.out
                .push((origin_tap.to_string(), name.to_string(), kind));
        }
        Ok(())
    }

    /// True when an installed dependency would be upgraded to its cutoff by
    /// its own plan entry: the same view the apply loop resolves, so a dep that
    /// is held, ahead, newer than its cutoff or pinned stays brew's.
    fn upgrades_this_run(&self, pkg: &Pkg) -> Result<bool, Error> {
        if pkg.pinned
            || pkg.kind != PkgKind::Formula
            || self.inv.class_for(&pkg.origin, &pkg.name, self.cfg) != PkgClass::Soaked
            || self.snaps.held_taps.contains_key(&pkg.origin)
            || (!origin::is_core_or_cask(&pkg.origin) && self.snaps.tap(&pkg.origin).is_none())
        {
            return Ok(false);
        }
        let view = resolve_view(
            self.brew,
            self.git,
            self.snaps,
            self.cache,
            &pkg.origin,
            &pkg.name,
            pkg.kind,
            Some(&pkg.receipt_rb),
        )?;
        Ok(view
            .is_some_and(|v| v.action == DesiredAction::InstallCutoff && v.cutoff_blob.is_some()))
    }

    /// (origin, name) a `brew deps --full-name` token resolves to, or None
    /// when brew keeps it. brew has already resolved bare names (core first,
    /// then a unique installed tap) and prints `user/repo/name` for a tap
    /// package, so a full name is that tap and a bare name is core or cask.
    fn dep_origin(&self, dep: &str) -> Result<Option<(String, String)>, Error> {
        if let Ok(tok) = inventory::parse_token(dep)
            && let Some(o) = tok.origin
        {
            return Ok(Some((o, tok.name)));
        }
        // At cutoff or HEAD: a dep born inside the window still resolves to
        // its origin, so the walk refuses the target instead of leaving the
        // name for brew to install unsoaked from HEAD.
        let exists = |o: &str, kind| -> Result<bool, Error> {
            let blobs = resolve_pkg_blobs(self.git, self.snaps, self.cache, o, dep, kind)?;
            Ok(blobs.cutoff.is_some() || blobs.head.is_some())
        };
        if exists(origin::CORE, PkgKind::Formula)? {
            return Ok(Some((origin::CORE.to_string(), dep.to_string())));
        }
        if exists(origin::CASK, PkgKind::Cask)? {
            return Ok(Some((origin::CASK.to_string(), dep.to_string())));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SoakHours;
    use crate::brew::{InstalledPkg, MockBrew};
    use crate::cmd::test_support::summary_has;
    use crate::flags;
    use std::collections::BTreeMap;

    use crate::git::InMemoryGit;

    use crate::origin::OriginRecords;
    use crate::resolve::PkgKind;
    use crate::snapshot::TapState;
    use crate::taps::TapInfo;
    use std::path::Path;

    use crate::cmd::RunResult;
    use crate::cmd::install::install;

    use crate::cmd::test_support::{
        PACKER_REFUSAL, cask_rb, cfg_with, cfg24, core_snaps, formula_pkg, formula_rb,
        hashicorp_tap, inv_from, lock_runs, packer_world, run_has_token, run_is_soaked_install,
        tap_snaps, tapped, unused_cache,
    };

    #[test]
    fn dep_closure_crosses_taps_and_core() {
        // terraform (hashicorp/tap) depends on `hashicorp/tap/helper` (brew prints a
        // tap package by full name) and bare `zlib` (core);
        // helper depends on `acme/tools/widget` (another soakable tap).
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
        git.insert_blob(
            "tapcut",
            "Formula/helper.rb",
            formula_rb("helper", "0.1.0", "h1"),
        );
        git.insert_blob(
            "taphead",
            "Formula/helper.rb",
            formula_rb("helper", "0.1.0", "h1"),
        );
        git.insert_blob(
            "acmecut",
            "Formula/widget.rb",
            formula_rb("widget", "2.0.0", "w1"),
        );
        git.insert_blob(
            "acmehead",
            "Formula/widget.rb",
            formula_rb("widget", "2.0.0", "w1"),
        );
        git.insert_tree("acmecut", &["Formula/widget.rb"]);
        git.insert_tree("acmehead", &["Formula/widget.rb"]);
        git.insert_blob(
            "cutoffsha",
            "Formula/z/zlib.rb",
            formula_rb("zlib", "1.3", "z1"),
        );
        git.insert_blob(
            "headsha",
            "Formula/z/zlib.rb",
            formula_rb("zlib", "1.3", "z1"),
        );
        let tap = tempfile::tempdir().unwrap();
        let staged = |p: &str| tap.path().join(p).to_string_lossy().into_owned();
        let mut deps = BTreeMap::new();
        deps.insert(
            staged("taps/hashicorp/tap/Formula/terraform.rb"),
            vec!["hashicorp/tap/helper".to_string(), "zlib".to_string()],
        );
        deps.insert(
            staged("taps/hashicorp/tap/Formula/helper.rb"),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/terraform.rb", "Formula/helper.rb"]);
        git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/helper.rb"]);
        snaps.taps.insert(
            "acme/tools".into(),
            TapState {
                hours: SoakHours::new(24).unwrap(),
                cutoff_sha: Some("acmecut".into()),
                head_sha: "acmehead".into(),
                cutoff_time: None,
            },
        );
        let cache = tempfile::tempdir().unwrap();
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
            &mut Vec::new(),
        )
        .unwrap();
        let runs = lock_runs(&brew);
        let installs: Vec<&str> = runs
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .map(|a| a.last().unwrap().as_str())
            .collect();
        assert_eq!(
            installs,
            vec![
                staged("taps/acme/tools/Formula/widget.rb"),
                staged("taps/hashicorp/tap/Formula/helper.rb"),
                staged("Formula/zlib.rb"),
                staged("taps/hashicorp/tap/Formula/terraform.rb"),
            ],
            "deps first, each from its own origin's staging dir: {runs:?}"
        );
        let records = OriginRecords::load(cache.path());
        assert_eq!(records.get(PkgKind::Formula, "widget"), Some("acme/tools"));
        assert_eq!(
            records.get(PkgKind::Formula, "helper"),
            Some("hashicorp/tap")
        );
        assert_eq!(records.get(PkgKind::Formula, "zlib"), None);
    }

    #[test]
    fn no_soak_and_unsoakable_deps_are_left_to_brew() {
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
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec![
                "ericfitz/tap/brewsoak".to_string(),
                "local/tap/thing".to_string(),
            ],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped(
                    "ericfitz/tap",
                    Some("https://github.com/ericfitz/homebrew-tap"),
                ),
                tapped("local/tap", None),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        let cache = tempfile::tempdir().unwrap();
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
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!r.refused);
        let runs = lock_runs(&brew);
        let installs: Vec<&Vec<String>> = runs
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .collect();
        assert_eq!(installs.len(), 1, "only the target: {runs:?}");
        assert!(!run_has_token(&runs, "ericfitz/tap/brewsoak"));
    }

    #[test]
    fn untrusted_tap_refusal_on_a_dep_holds_the_dep() {
        let (git, snaps) = packer_world();
        git.insert_blob(
            "tapcut",
            "Formula/widget.rb",
            formula_rb("widget", "1.0.0", "w"),
        );
        git.insert_blob(
            "taphead",
            "Formula/widget.rb",
            formula_rb("widget", "1.0.0", "w"),
        );
        git.insert_tree("tapcut", &["Formula/packer.rb", "Formula/widget.rb"]);
        git.insert_tree("taphead", &["Formula/packer.rb", "Formula/widget.rb"]);
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/packer.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["hashicorp/tap/widget".to_string()],
        );
        let widget_refusal = String::from_utf8_lossy(PACKER_REFUSAL).replace("packer", "widget");
        let brew = MockBrew {
            deps,
            taps: hashicorp_tap(),
            next_outputs: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                (1, widget_refusal.into_bytes()),
                (1, PACKER_REFUSAL.to_vec()),
            ])),
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
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
            &["hashicorp/tap/packer".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert_eq!(r.brew_status, None);
        assert_eq!(
            text.matches("brew does not trust tap").count(),
            1,
            "one hold line for the target, not one per package: {text}"
        );
        assert!(
            text.contains(
                "packer: dependency widget: brew does not trust tap hashicorp/tap; run brew trust hashicorp/tap, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(
            summary_has(&text, "held", "1"),
            "the target is held once: {text}"
        );
        assert!(summary_has(&text, "upgraded", "0"), "{text}");
    }

    #[test]
    fn deps_failure_on_staged_tap_formula_holds_with_note() {
        struct FailDepsBrew(MockBrew);
        impl Brew for FailDepsBrew {
            fn brew_bin(&self) -> &Path {
                self.0.brew_bin()
            }
            fn run(&self, a: &[String]) -> Result<std::process::Output, Error> {
                self.0.run(a)
            }
            fn run_visible(
                &self,
                a: &[String],
                t: &mut dyn std::io::Write,
            ) -> Result<std::process::Output, Error> {
                self.0.run_visible(a, t)
            }
            fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
                self.0.installed_packages()
            }
            fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
                self.0.tap_info()
            }
            fn outdated_names(&self, greedy: flags::Greedy) -> Result<Vec<String>, Error> {
                self.0.outdated_names(greedy)
            }
            fn deps(&self, _k: PkgKind, token: &str) -> Result<Vec<String>, Error> {
                Err(Error::Brew {
                    status: 1,
                    message: format!(
                        "Error: No available formula with the name \"helper\" ({token})"
                    ),
                })
            }
        }
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
        let brew = FailDepsBrew(MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        });
        let cfg = cfg24();
        let inv = inv_from(&brew.0, &cfg);
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
        assert!(r.refused);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("cannot be installed from a staged copy")
        );
        assert!(
            lock_runs(&brew.0)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install"))
        );
    }

    #[test]
    fn deps_untrusted_tap_refusal_holds_with_trust_note() {
        struct FailDepsBrew(MockBrew);
        impl Brew for FailDepsBrew {
            fn brew_bin(&self) -> &Path {
                self.0.brew_bin()
            }
            fn run(&self, a: &[String]) -> Result<std::process::Output, Error> {
                self.0.run(a)
            }
            fn run_visible(
                &self,
                a: &[String],
                t: &mut dyn std::io::Write,
            ) -> Result<std::process::Output, Error> {
                self.0.run_visible(a, t)
            }
            fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
                self.0.installed_packages()
            }
            fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
                self.0.tap_info()
            }
            fn outdated_names(&self, greedy: flags::Greedy) -> Result<Vec<String>, Error> {
                self.0.outdated_names(greedy)
            }
            fn deps(&self, _k: PkgKind, token: &str) -> Result<Vec<String>, Error> {
                Err(Error::Brew {
                    status: 1,
                    message: format!(
                        "Error: terraform: Refusing to load formula hashicorp/tap/terraform from untrusted tap hashicorp/tap. ({token})"
                    ),
                })
            }
        }
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
        let brew = FailDepsBrew(MockBrew {
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        });
        let cfg = cfg24();
        let inv = inv_from(&brew.0, &cfg);
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
        assert!(r.refused);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "terraform: brew does not trust tap hashicorp/tap; run brew trust hashicorp/tap, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(!text.contains("staged copy"), "{text}");
        assert!(
            lock_runs(&brew.0)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install"))
        );
    }

    #[test]
    fn dep_in_held_tap_refuses_the_target() {
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
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        snaps.held_taps.insert(
            "acme/tools".into(),
            "while fetching history from origin, git failed:\nfatal: boom".into(),
        );
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
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("cannot install terraform: dependency widget")
                && text.contains("acme/tools could not be refreshed"),
            "{text}"
        );
        assert!(
            summary_has(&text, "upgraded", "0") && summary_has(&text, "held", "1"),
            "a held dependency holds the target, it does not upgrade it: {text}"
        );
    }

    #[test]
    fn dep_in_tap_without_snapshot_refuses_target_and_run_continues() {
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
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
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
            &["hashicorp/tap/terraform".into(), "alpha".into()],
            false,
            false,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(r.refused);
        assert!(
            text.contains("cannot install terraform: dependency widget")
                && text.contains("acme/tools has no soak snapshot"),
            "{text}"
        );
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "alpha"),
            "run continued"
        );
        assert!(!run_is_soaked_install(&lock_runs(&brew), "terraform"));
    }

    #[test]
    fn explicit_tap_dep_too_new_refuses_target_as_too_new() {
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
        git.insert_blob(
            "acmehead",
            "Formula/widget.rb",
            formula_rb("widget", "2.0.0", "w1"),
        );
        git.insert_tree("acmecut", &["README.md"]);
        git.insert_tree("acmehead", &["Formula/widget.rb"]);
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        snaps.taps.insert(
            "acme/tools".into(),
            TapState {
                hours: SoakHours::new(24).unwrap(),
                cutoff_sha: Some("acmecut".into()),
                head_sha: "acmehead".into(),
                cutoff_time: None,
            },
        );
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            tempfile::tempdir().unwrap().path(),
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
        assert!(
            text.contains("cannot install terraform: dependency widget is too new"),
            "{text}"
        );
        assert!(
            lock_runs(&brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install"))
        );
    }

    /// A tap target with one bare dep `helper`; `setup` stages the git side.
    fn bare_dep_refusal(
        dep: &str,
        setup: impl FnOnce(&InMemoryGit),
    ) -> (String, RunResult, MockBrew) {
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
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec![dep.to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        setup(&git);
        let mut out = Vec::new();
        let r = install(
            &brew,
            &git,
            &snaps,
            tempfile::tempdir().unwrap().path(),
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
        (String::from_utf8(out).unwrap(), r, brew)
    }

    fn assert_dep_too_new_refusal(text: &str, r: &RunResult, brew: &MockBrew) {
        assert!(r.refused, "{text}");
        assert!(
            text.contains("cannot install terraform: dependency helper is too new"),
            "{text}"
        );
        assert!(
            lock_runs(brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install")),
            "nothing may be installed: {:?}",
            lock_runs(brew)
        );
        assert!(
            summary_has(text, "upgraded", "0") && summary_has(text, "held", "1"),
            "a dep refusal holds the target, it does not upgrade it: {text}"
        );
    }

    #[test]
    fn tap_dep_by_full_name_only_at_tap_head_refuses_target_as_too_new() {
        let (text, r, brew) = bare_dep_refusal("hashicorp/tap/helper", |git| {
            git.insert_blob(
                "taphead",
                "Formula/helper.rb",
                formula_rb("helper", "0.1.0", "h1"),
            );
            git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/helper.rb"]);
        });
        assert_dep_too_new_refusal(&text, &r, &brew);
    }

    #[test]
    fn bare_dep_only_at_core_head_refuses_target_as_too_new() {
        let (text, r, brew) = bare_dep_refusal("helper", |git| {
            git.insert_blob(
                "headsha",
                "Formula/h/helper.rb",
                formula_rb("helper", "0.1.0", "h1"),
            );
        });
        assert_dep_too_new_refusal(&text, &r, &brew);
    }

    #[test]
    fn bare_dep_name_is_core_even_when_the_dependents_tap_has_it() {
        // brew resolves a bare dep name core first and prints a tap package
        // by full name, so a bare `zlib` is core's, never the dependent's tap's.
        let git = InMemoryGit::new();
        for sha in ["tapcut", "taphead"] {
            git.insert_blob(
                sha,
                "Formula/terraform.rb",
                formula_rb("terraform", "1.1.0", "midsha"),
            );
            git.insert_blob(sha, "Formula/zlib.rb", formula_rb("zlib", "9.9", "tapz"));
        }
        for sha in ["cutoffsha", "headsha"] {
            git.insert_blob(sha, "Formula/z/zlib.rb", formula_rb("zlib", "1.3", "z1"));
        }
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["zlib".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/terraform.rb", "Formula/zlib.rb"]);
        git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/zlib.rb"]);
        let cache = tempfile::tempdir().unwrap();
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
            &mut Vec::new(),
        )
        .unwrap();
        let installs: Vec<String> = lock_runs(&brew)
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .map(|a| a.last().unwrap().clone())
            .collect();
        assert_eq!(installs.len(), 2, "{installs:?}");
        assert!(
            installs[0].ends_with("zlib.rb") && !installs[0].contains("taps/hashicorp"),
            "the bare dep is core's: {installs:?}"
        );
    }

    #[test]
    fn ambiguous_dep_name_holds_the_target_with_brews_message() {
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
        let tap = tempfile::tempdir().unwrap();
        let mut deps_errors = BTreeMap::new();
        deps_errors.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            "Error: Formulae found in multiple taps:\n       * a/tap/cats\n       * b/tap/cats\n\nPlease use the fully-qualified name (e.g. a/tap/cats) to refer to a specific formula.".to_string(),
        );
        let brew = MockBrew {
            deps_errors,
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
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("cannot install terraform: a dependency name is ambiguous")
                && text.contains("Formulae found in multiple taps")
                && text.contains("a/tap/cats")
                && summary_has(&text, "upgraded", "0")
                && summary_has(&text, "held", "1"),
            "{text}"
        );
        assert!(
            lock_runs(&brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("install"))
        );
    }

    #[test]
    fn installed_dep_from_held_tap_does_not_refuse_target() {
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
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["acme/tools/widget".to_string()],
        );
        let brew = MockBrew {
            deps,
            installed: vec![formula_pkg("widget", formula_rb("widget", "2.0.0", "w1"))],
            taps: vec![
                tapped(
                    "hashicorp/tap",
                    Some("https://github.com/hashicorp/homebrew-tap"),
                ),
                tapped("acme/tools", Some("https://github.com/acme/homebrew-tools")),
            ],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        snaps
            .held_taps
            .insert("acme/tools".into(), "fatal: boom".into());
        let cache = tempfile::tempdir().unwrap();
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
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!r.refused);
        assert!(run_is_soaked_install(&lock_runs(&brew), "terraform"));
    }

    #[test]
    fn dependency_cycle_terminates_and_installs_dep_first() {
        let git = InMemoryGit::new();
        for (sha, ver) in [("tapcut", "1.1.0"), ("taphead", "1.2.0")] {
            git.insert_blob(
                sha,
                "Formula/terraform.rb",
                formula_rb("terraform", ver, "s1"),
            );
            git.insert_blob(
                sha,
                "Formula/helper.rb",
                formula_rb("helper", "0.1.0", "h1"),
            );
        }
        let tap = tempfile::tempdir().unwrap();
        let mut deps = BTreeMap::new();
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/terraform.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["hashicorp/tap/helper".to_string()],
        );
        deps.insert(
            tap.path()
                .join("taps/hashicorp/tap/Formula/helper.rb")
                .to_string_lossy()
                .into_owned(),
            vec!["hashicorp/tap/terraform".to_string()],
        );
        let brew = MockBrew {
            deps,
            taps: vec![tapped(
                "hashicorp/tap",
                Some("https://github.com/hashicorp/homebrew-tap"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/terraform.rb", "Formula/helper.rb"]);
        git.insert_tree("taphead", &["Formula/terraform.rb", "Formula/helper.rb"]);
        let cache = tempfile::tempdir().unwrap();
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
            &mut Vec::new(),
        )
        .unwrap();
        let installs: Vec<String> = lock_runs(&brew)
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .map(|a| a.last().unwrap().clone())
            .collect();
        assert_eq!(installs.len(), 2, "{installs:?}");
        assert!(
            installs[0].ends_with("helper.rb") && installs[1].ends_with("terraform.rb"),
            "{installs:?}"
        );
    }

    #[test]
    fn install_refuses_target_when_dep_yanked() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let lib_mid = formula_rb("lib", "1.0.0", "libsha");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        git.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);

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
            !run_has_token(&runs, "brewsoakr/soaked/fresh"),
            "yanked dep must refuse target install: {runs:?}"
        );
        assert!(result.refused, "missing yanked dep refuses the target");
    }

    fn run_is_soaked_dep_install(runs: &[Vec<String>], name: &str, kind_flag: &str) -> bool {
        let suffix = format!("/{name}.rb");
        runs.iter().any(|args| {
            args.first().map(String::as_str) == Some("install")
                && args.iter().any(|a| a.ends_with(&suffix))
                && args.iter().any(|a| a == kind_flag)
                && !args.iter().any(|a| a == "--ignore-dependencies")
        })
    }

    struct FailShowGit {
        inner: InMemoryGit,
        fail_substr: &'static str,
    }

    impl GitStore for FailShowGit {
        fn init_bare(&self, dir: &Path) -> Result<(), Error> {
            self.inner.init_bare(dir)
        }

        fn fetch_depth1(
            &self,
            dir: &Path,
            remote: &str,
            sha: &str,
            ref_name: &str,
        ) -> Result<(), Error> {
            self.inner.fetch_depth1(dir, remote, sha, ref_name)
        }

        fn show(&self, dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, Error> {
            if path.contains(self.fail_substr) {
                return Err(Error::Other(format!("git show failed: {path}")));
            }
            self.inner.show(dir, sha, path)
        }

        fn rev_parse(&self, dir: &Path, rev: &str) -> Result<Option<String>, Error> {
            self.inner.rev_parse(dir, rev)
        }

        fn gc_prune(&self, dir: &Path) -> Result<(), Error> {
            self.inner.gc_prune(dir)
        }
    }

    #[test]
    fn install_uses_cutoff_tap_deps_not_head_graph() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let lib_mid = formula_rb("lib", "1.0.0", "libmid");
        let lib_new = formula_rb("lib", "1.1.0", "libnew");
        let head_mid = formula_rb("headonly", "1.0.0", "hmid");
        let head_new = formula_rb("headonly", "1.1.0", "hnew");

        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        git.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        git.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);
        git.insert_blob("headsha", "Formula/lib/lib.rb", lib_new);
        git.insert_blob("cutoffsha", "Formula/h/headonly.rb", head_mid);
        git.insert_blob("headsha", "Formula/h/headonly.rb", head_new);

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("fresh".into(), vec!["headonly".into()]);
        deps.insert("fresh".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
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
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_dep_install(&runs, "lib", "--formula"),
            "cutoff dep lib must be installed from tap: {runs:?}"
        );
        assert!(
            run_is_soaked_install(&runs, "fresh"),
            "target must still install: {runs:?}"
        );
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/headonly"),
            "HEAD-only dep must not enter soak path: {runs:?}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn install_cask_installs_formula_dep() {
        let app_mid = cask_rb("app", "1.0.0");
        let app_new = cask_rb("app", "1.1.0");
        let lib_mid = formula_rb("lib", "1.0.0", "libmid");
        let lib_new = formula_rb("lib", "1.1.0", "libnew");

        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/a/app.rb", app_mid);
        git.insert_blob("caskhead", "Casks/a/app.rb", app_new);
        git.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);
        git.insert_blob("headsha", "Formula/lib/lib.rb", lib_new);

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("app".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["app".to_string()];
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
            true,
            false,
            &[],
            &mut out,
        )
        .expect("install");
        let runs = lock_runs(&brew);
        assert!(
            run_is_soaked_dep_install(&runs, "lib", "--formula"),
            "cask must soak-install formula dep: {runs:?}"
        );
        assert!(
            run_is_soaked_install(&runs, "app"),
            "cask target must path-install without --ignore-dependencies: {runs:?}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn install_git_show_error_aborts_before_target() {
        let fresh_mid = formula_rb("fresh", "1.1.0", "midsha");
        let fresh_new = formula_rb("fresh", "1.2.0", "newsha");
        let lib_mid = formula_rb("lib", "1.0.0", "libmid");
        let lib_new = formula_rb("lib", "1.1.0", "libnew");

        let inner = InMemoryGit::new();
        inner.insert_blob("cutoffsha", "Formula/f/fresh.rb", fresh_mid);
        inner.insert_blob("headsha", "Formula/f/fresh.rb", fresh_new);
        inner.insert_blob("cutoffsha", "Formula/lib/lib.rb", lib_mid);
        inner.insert_blob("headsha", "Formula/lib/lib.rb", lib_new);
        let git = FailShowGit {
            inner,
            fail_substr: "Formula/lib/lib.rb",
        };

        let mut deps = std::collections::BTreeMap::new();
        deps.insert("fresh".into(), vec!["lib".into()]);
        deps.insert("fresh".into(), vec!["lib".into()]);
        let brew = MockBrew {
            deps,
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        let names = ["fresh".to_string()];
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let err = install(
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
        .expect_err("git show failure must abort the command");
        assert!(
            matches!(err, Error::Other(ref msg) if msg.contains("git show failed")),
            "{err}"
        );
        let runs = lock_runs(&brew);
        assert!(
            !run_has_token(&runs, "brewsoakr/soaked/fresh"),
            "must not install the target after git failure: {runs:?}"
        );
    }
}
