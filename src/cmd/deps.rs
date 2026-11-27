use super::view::{cutoff_blob_exists, natural_kind, resolve_pkg_blobs, resolve_view};
use crate::Error;
use crate::brew::{Brew, NameQuery};
use crate::config::Config;
use crate::digest::sha256_hex;
use crate::eligibility::DesiredAction;
use crate::git::GitStore;
use crate::inventory::{self, Inventory, Pkg, PkgClass};
use crate::origin::{self};
use crate::resolve::PkgKind;
use crate::snapshot::Snapshots;
use crate::taps;
use std::collections::HashSet;
use std::path::Path;

/// A dependency in a tap that could not be refreshed this run.
pub(super) struct HeldDep {
    pub(super) dep: String,
    /// Why the dep's tap cannot be used, e.g. `tap acme/tools could not be
    /// refreshed`; git detail follows on its own lines.
    pub(super) reason: String,
}

/// Why the by-name gate holds a member of the closure.
pub(super) enum NameWhy {
    /// brew's own definition of the name is not the soaked cutoff.
    NotCutoff { brew_version: String },
    /// A bare install would load the name from another tap.
    OtherTap { tap: String, origin: String },
    /// brew refuses to load the name from this untrusted tap.
    Untrusted(String),
    /// brew's loaders could not describe the name (first line of its text).
    Lookup(String),
    /// brew could not list the member's dependencies (first line of its text).
    Deps(String),
    /// The member has no soaked cutoff to compare against.
    NoCutoff,
}

impl NameWhy {
    /// The reason as it reads in a hold line and a note.
    pub(super) fn text(&self) -> String {
        match self {
            Self::NotCutoff { brew_version } => {
                format!("brew would install {brew_version}, which is not the soaked cutoff")
            }
            Self::OtherTap { tap, origin } => {
                format!("brew resolves the name to {tap}, not {origin}")
            }
            Self::Untrusted(tap) => format!("brew does not trust tap {tap}"),
            Self::Lookup(first) => format!("brew could not describe it by name ({first})"),
            Self::Deps(first) => format!("brew could not read its dependencies ({first})"),
            Self::NoCutoff => "it has no soaked cutoff yet".to_string(),
        }
    }
}

/// The member of the closure that holds the target, and why. `dep` is None
/// when the member is the target itself.
pub(super) struct NameHold {
    pub(super) dep: Option<String>,
    pub(super) why: NameWhy,
}

pub(super) struct CutoffDeps {
    /// (origin, name, kind), dependencies first.
    pub(super) deps: Vec<(String, String, PkgKind)>,
    pub(super) held: Option<HeldDep>,
    pub(super) by_name: Option<NameHold>,
}

/// What a member of the closure is for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    /// The target: must pass the gate.
    Root,
    /// Brewsoak installs it, dependencies first: must pass the gate.
    Install,
    /// Installed and left alone by this run: passes when brew has its
    /// latest version installed or the gate passes.
    Check,
}

pub(super) struct CutoffDepWalk<'a, B, G> {
    pub(super) brew: &'a B,
    pub(super) git: &'a G,
    pub(super) snaps: &'a Snapshots,
    pub(super) cache: &'a Path,
    pub(super) inv: &'a Inventory,
    pub(super) cfg: &'a Config,
    pub(super) visiting: HashSet<String>,
    pub(super) visited: HashSet<String>,
    pub(super) out: Vec<(String, String, PkgKind)>,
    pub(super) held: Option<HeldDep>,
    pub(super) by_name: Option<NameHold>,
}

/// First line of brew's text, for a hold line.
pub(super) fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no detail")
        .to_string()
}

impl<B: Brew, G: GitStore> CutoffDepWalk<'_, B, G> {
    fn hold(&mut self, role: Role, name: &str, why: NameWhy) {
        let dep = (role != Role::Root).then(|| name.to_string());
        self.by_name = Some(NameHold { dep, why });
    }

    fn failed(&self) -> bool {
        self.held.is_some() || self.by_name.is_some()
    }

    pub(super) fn visit(
        &mut self,
        origin_tap: &str,
        name: &str,
        kind: PkgKind,
        role: Role,
    ) -> Result<(), Error> {
        let key = format!("{origin_tap}/{name}");
        if self.visiting.contains(&key) || self.visited.contains(&key) {
            return Ok(());
        }
        self.visiting.insert(key.clone());
        // A third-party root asks for its dependencies first, so brew's
        // refusal to load an untrusted tap keeps its own note. The list is
        // used only after the gate below passes.
        let early = if role == Role::Root && !origin::is_core_or_cask(origin_tap) {
            Some(self.brew.deps(kind, name)?)
        } else {
            None
        };
        let blobs = resolve_pkg_blobs(self.git, self.snaps, self.cache, origin_tap, name, kind)?;
        let query = NameQuery {
            kind,
            name: name.to_string(),
            origin: origin_tap.to_ascii_lowercase(),
            cutoff_sha256: blobs.cutoff.as_deref().map(sha256_hex),
        };
        let has_cutoff = query.cutoff_sha256.is_some();
        let answer = self
            .brew
            .by_name(std::slice::from_ref(&query))?
            .into_iter()
            .next()
            .unwrap_or_else(|| Err("brew gave no answer for this name".to_string()));
        let answer = match answer {
            Ok(a) => a,
            Err(text) => {
                let why = match taps::untrusted_tap(&text) {
                    Some(tap) => NameWhy::Untrusted(tap),
                    None => NameWhy::Lookup(first_line(&text)),
                };
                self.hold(role, name, why);
                return Ok(());
            }
        };
        let installs = answer.installs_cutoff(origin_tap);
        // brew leaves an installed dependency at its current version alone,
        // so one this run would upgrade only needs the gate when brew would
        // change it.
        let role = if role == Role::Install
            && answer.installed_current
            && !installs
            && self.inv.find(name).is_some()
        {
            Role::Check
        } else {
            role
        };
        let passes = installs || (role == Role::Check && answer.installed_current);
        if !passes {
            let why = if !answer.tap.eq_ignore_ascii_case(origin_tap) {
                NameWhy::OtherTap {
                    tap: answer.tap,
                    origin: origin_tap.to_ascii_lowercase(),
                }
            } else if !has_cutoff {
                NameWhy::NoCutoff
            } else {
                NameWhy::NotCutoff {
                    brew_version: answer.version,
                }
            };
            self.hold(role, name, why);
            return Ok(());
        }
        let listed = match early {
            Some(list) => list,
            None => match self.brew.deps(kind, name) {
                Ok(list) => list,
                Err(Error::Brew { message, .. })
                    if role != Role::Root && !taps::ambiguous_formula(&message) =>
                {
                    let why = match taps::untrusted_tap(&message) {
                        Some(tap) => NameWhy::Untrusted(tap),
                        None => NameWhy::Deps(first_line(&message)),
                    };
                    self.hold(role, name, why);
                    return Ok(());
                }
                Err(e) => return Err(e),
            },
        };
        for dep in listed {
            // Already installed, from any origin. One this run upgrades goes
            // first, so the dependent never links against a library the
            // dependency has not yet shipped; any other is walked through,
            // because brew expands the dependencies of a dependency it skips.
            let installed_name = inventory::parse_token(&dep)
                .map(|t| t.name)
                .unwrap_or_else(|_| dep.clone());
            if let Some(pkg) = self.inv.find(&installed_name) {
                if pkg.kind == PkgKind::Cask
                    || self.inv.class_for(&pkg.origin, &pkg.name, self.cfg) != PkgClass::Soaked
                    || self.snaps.held_taps.contains_key(&pkg.origin)
                    || (!origin::is_core_or_cask(&pkg.origin)
                        && self.snaps.tap(&pkg.origin).is_none())
                {
                    continue; // brew's, as today
                }
                let child = if self.upgrades_this_run(pkg)? {
                    Role::Install
                } else {
                    Role::Check
                };
                let (o, k) = (pkg.origin.clone(), pkg.kind);
                self.visit(&o, &installed_name, k, child)?;
                if self.failed() {
                    return Ok(());
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
                // Its cutoff is unknown: refuse before asking brew anything.
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
                // Nothing to compare or install: install_missing_dep refuses
                // the target from the dep's upstream status (too new, not
                // found).
                self.out.push((dep_origin, dep_name, dep_kind));
                continue;
            }
            self.visit(&dep_origin, &dep_name, dep_kind, Role::Install)?;
            if self.failed() {
                return Ok(());
            }
        }
        self.visiting.remove(&key);
        self.visited.insert(key);
        if role == Role::Install {
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
            "terraform".to_string(),
            vec!["hashicorp/tap/helper".to_string(), "zlib".to_string()],
        );
        deps.insert("helper".to_string(), vec!["acme/tools/widget".to_string()]);
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
            "terraform".to_string(),
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
            "packer".to_string(),
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
            fn run_visible_in(
                &self,
                a: &[String],
                t: &mut dyn std::io::Write,
                ctx: &crate::quiet::RunContext,
            ) -> Result<std::process::Output, Error> {
                self.0.run_visible_in(a, t, ctx)
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
            fn by_name(
                &self,
                q: &[crate::brew::NameQuery],
            ) -> Result<Vec<Result<crate::brew::ByName, String>>, Error> {
                self.0.by_name(q)
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
        let text = String::from_utf8(out).unwrap();
        // The error text carries the token the walk asked about: the bare name.
        assert!(
            text.contains(
                "held terraform: brew could not read its dependencies (Error: No available formula with the name \"helper\" (terraform))"
            ),
            "{text}"
        );
        assert!(
            text.contains("use `brew install hashicorp/tap/terraform` to bypass brewsoak."),
            "{text}"
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
            fn run_visible_in(
                &self,
                a: &[String],
                t: &mut dyn std::io::Write,
                ctx: &crate::quiet::RunContext,
            ) -> Result<std::process::Output, Error> {
                self.0.run_visible_in(a, t, ctx)
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
            fn by_name(
                &self,
                q: &[crate::brew::NameQuery],
            ) -> Result<Vec<Result<crate::brew::ByName, String>>, Error> {
                self.0.by_name(q)
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
            "terraform".to_string(),
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
            "terraform".to_string(),
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
            "terraform".to_string(),
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
        deps.insert("terraform".to_string(), vec![dep.to_string()]);
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
        deps.insert("terraform".to_string(), vec!["zlib".to_string()]);
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
            "terraform".to_string(),
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
            "terraform".to_string(),
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
            "terraform".to_string(),
            vec!["hashicorp/tap/helper".to_string()],
        );
        deps.insert(
            "helper".to_string(),
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

#[cfg(test)]
mod gate_tests {
    use crate::Error;
    use crate::brew::{Brew, ByName, InstalledPkg, MockBrew, NameQuery};
    use crate::cmd::RunResult;
    use crate::cmd::install::install;
    use crate::cmd::test_support::{
        cask_rb, cfg24, core_snaps, formula_pkg, formula_pkg_from, formula_rb, inv_from, lock_runs,
        run_is_soaked_install, tap_snaps, tapped, unused_cache,
    };
    use crate::cmd::upgrade::upgrade;
    use crate::digest::sha256_hex;
    use crate::flags;
    use crate::git::InMemoryGit;
    use crate::resolve::PkgKind;
    use crate::taps::TapInfo;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Mutex;

    const CORE: &str = "homebrew/core";

    /// Core formulae with a 1.1.0 cutoff and a 1.2.0 head each.
    fn core_git(names: &[&str]) -> InMemoryGit {
        let git = InMemoryGit::new();
        for name in names {
            let dir = if name.starts_with("lib") {
                "lib"
            } else {
                &name[..1]
            };
            git.insert_blob(
                "cutoffsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.1.0", "midsha"),
            );
            git.insert_blob(
                "headsha",
                &format!("Formula/{dir}/{name}.rb"),
                formula_rb(name, "1.2.0", "newsha"),
            );
        }
        git
    }

    fn outdated(name: &str) -> InstalledPkg {
        formula_pkg(name, formula_rb(name, "1.0.0", "oldsha"))
    }

    fn at_cutoff(name: &str) -> InstalledPkg {
        formula_pkg(name, formula_rb(name, "1.1.0", "midsha"))
    }

    fn answer(tap: &str, version: &str, same_source: bool, installed_current: bool) -> ByName {
        ByName {
            tap: tap.into(),
            version: version.into(),
            same_source,
            installed_current,
        }
    }

    fn deps_of(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    v.iter().map(|d| (*d).to_string()).collect(),
                )
            })
            .collect()
    }

    fn run(
        brew: &MockBrew,
        git: &InMemoryGit,
        names: &[&str],
        user_flags: &[&str],
    ) -> (RunResult, String) {
        let names: Vec<String> = names.iter().map(|s| (*s).to_string()).collect();
        let user_flags: Vec<String> = user_flags.iter().map(|s| (*s).to_string()).collect();
        let tap = tempfile::tempdir().expect("tap");
        let cfg = cfg24();
        let inv = inv_from(brew, &cfg);
        let mut out = Vec::new();
        let r = upgrade(
            brew,
            git,
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &names,
            &user_flags,
            &mut out,
        )
        .expect("a brew failure holds the target; the call must not fail");
        (r, String::from_utf8(out).expect("utf8"))
    }

    fn installs(brew: &MockBrew) -> Vec<Vec<String>> {
        lock_runs(brew)
            .into_iter()
            .chain(brew.visible_runs.lock().expect("visible").clone())
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .collect()
    }

    fn summary(text: &str, label: &str, value: &str) -> bool {
        crate::cmd::test_support::summary_has(text, label, value)
    }

    struct RecordBrew {
        inner: MockBrew,
        tokens: Mutex<Vec<String>>,
    }

    impl Brew for RecordBrew {
        fn brew_bin(&self) -> &Path {
            self.inner.brew_bin()
        }
        fn run(&self, a: &[String]) -> Result<std::process::Output, Error> {
            self.inner.run(a)
        }
        fn run_visible_in(
            &self,
            a: &[String],
            t: &mut dyn std::io::Write,
            ctx: &crate::quiet::RunContext,
        ) -> Result<std::process::Output, Error> {
            self.inner.run_visible_in(a, t, ctx)
        }
        fn installed_packages(&self) -> Result<Vec<InstalledPkg>, Error> {
            self.inner.installed_packages()
        }
        fn tap_info(&self) -> Result<Vec<TapInfo>, Error> {
            self.inner.tap_info()
        }
        fn outdated_names(&self, greedy: flags::Greedy) -> Result<Vec<String>, Error> {
            self.inner.outdated_names(greedy)
        }
        fn by_name(&self, q: &[NameQuery]) -> Result<Vec<Result<ByName, String>>, Error> {
            self.inner.by_name(q)
        }
        fn deps(&self, kind: PkgKind, token: &str) -> Result<Vec<String>, Error> {
            self.tokens.lock().expect("tokens").push(token.to_string());
            self.inner.deps(kind, token)
        }
    }

    #[test]
    fn walk_asks_deps_by_bare_name() {
        let git = core_git(&["alpha", "libx"]);
        let brew = RecordBrew {
            inner: MockBrew {
                installed: vec![outdated("alpha")],
                deps: deps_of(&[("alpha", &["libx"])]),
                ..MockBrew::new()
            },
            tokens: Mutex::new(Vec::new()),
        };
        let cfg = cfg24();
        let inv = inv_from(&brew.inner, &cfg);
        let tap = tempfile::tempdir().expect("tap");
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            tap.path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("upgrade");
        let tokens = brew.tokens.lock().expect("tokens").clone();
        assert!(!tokens.is_empty(), "the walk asked brew nothing");
        for t in &tokens {
            assert!(!t.contains('/') && !t.ends_with(".rb"), "{tokens:?}");
        }
    }

    #[test]
    fn by_name_is_asked_with_the_origin_and_the_raw_cutoff_hash() {
        let git = core_git(&["alpha"]);
        let brew = MockBrew {
            installed: vec![outdated("alpha")],
            ..MockBrew::new()
        };
        run(&brew, &git, &["alpha"], &[]);
        let want = sha256_hex(formula_rb("alpha", "1.1.0", "midsha").as_bytes());
        let seen = brew.by_name_queries.lock().expect("queries").clone();
        let q = seen.iter().find(|q| q.name == "alpha").expect("asked");
        assert_eq!(q.origin, CORE);
        assert_eq!(q.kind, PkgKind::Formula);
        assert_eq!(q.cutoff_sha256.as_deref(), Some(want.as_str()));
    }

    #[test]
    fn deps_failure_on_a_core_formula_holds_it_and_the_run_continues() {
        let git = core_git(&["alpha", "delta"]);
        let mut deps_errors = BTreeMap::new();
        deps_errors.insert(
            "alpha".to_string(),
            "Error: No available formula with the name \"alpha\".".to_string(),
        );
        let brew = MockBrew {
            installed: vec![outdated("alpha"), outdated("delta")],
            deps_errors,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &[], &[]);
        assert!(r.refused, "{text}");
        assert!(
            text.contains(
                "held alpha: brew could not read its dependencies (Error: No available formula with the name \"alpha\".)"
            ),
            "{text}"
        );
        assert!(text.contains("issue #17"), "{text}");
        assert!(run_is_soaked_install(&lock_runs(&brew), "delta"), "{text}");
        assert!(!run_is_soaked_install(&lock_runs(&brew), "alpha"), "{text}");
        assert!(summary(&text, "held", "1"), "{text}");
        assert!(summary(&text, "upgraded", "1"), "{text}");
    }

    #[test]
    fn by_name_lookup_failure_holds_the_target_and_the_run_continues() {
        let git = core_git(&["alpha", "delta"]);
        let mut by_name = BTreeMap::new();
        by_name.insert(
            "alpha".to_string(),
            Err("FormulaUnavailableError: No available formula\nmore detail".to_string()),
        );
        let brew = MockBrew {
            installed: vec![outdated("alpha"), outdated("delta")],
            by_name,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &[], &[]);
        assert!(r.refused, "{text}");
        assert!(
            text.contains(
                "held alpha: brew could not describe it by name (FormulaUnavailableError: No available formula)"
            ),
            "{text}"
        );
        assert!(!text.contains("more detail"), "{text}");
        assert!(run_is_soaked_install(&lock_runs(&brew), "delta"), "{text}");
        assert!(summary(&text, "held", "1"), "{text}");
    }

    #[test]
    fn missing_dep_not_at_brew_current_holds_the_target() {
        let git = core_git(&["alpha", "libx"]);
        let mut by_name = BTreeMap::new();
        by_name.insert("libx".to_string(), Ok(answer(CORE, "1.2.0", false, false)));
        let brew = MockBrew {
            installed: vec![outdated("alpha")],
            deps: deps_of(&[("alpha", &["libx"])]),
            by_name,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &["alpha"], &[]);
        assert!(r.refused, "{text}");
        assert!(
            text.contains(
                "alpha: dependency libx: brew would install 1.2.0, which is not the soaked cutoff; brewsoak cannot install a soaked copy until issue #17 is fixed; use `brew upgrade alpha` to bypass brewsoak."
            ),
            "{text}"
        );
        assert!(installs(&brew).is_empty(), "{:?}", installs(&brew));
        assert!(summary(&text, "held", "1"), "{text}");
        assert!(summary(&text, "upgraded", "0"), "{text}");
    }

    #[test]
    fn outdated_installed_dep_not_at_cutoff_holds_the_target() {
        let git = core_git(&["alpha", "libx"]);
        let mut by_name = BTreeMap::new();
        by_name.insert("libx".to_string(), Ok(answer(CORE, "1.2.0", false, false)));
        let brew = MockBrew {
            installed: vec![outdated("alpha"), at_cutoff("libx")],
            deps: deps_of(&[("alpha", &["libx"])]),
            by_name,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &["alpha"], &[]);
        assert!(r.refused, "{text}");
        assert!(
            text.contains("alpha: dependency libx: brew would install 1.2.0"),
            "{text}"
        );
        assert!(installs(&brew).is_empty(), "{:?}", installs(&brew));
    }

    #[test]
    fn installed_current_dep_is_left_alone_and_walked_through() {
        let git = core_git(&["alpha", "libx", "liby"]);
        let mut by_name = BTreeMap::new();
        by_name.insert("libx".to_string(), Ok(answer(CORE, "1.2.0", false, true)));
        by_name.insert("liby".to_string(), Ok(answer(CORE, "1.2.0", false, false)));
        let brew = MockBrew {
            installed: vec![outdated("alpha"), at_cutoff("libx")],
            deps: deps_of(&[("alpha", &["libx"]), ("libx", &["liby"])]),
            by_name,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &["alpha"], &[]);
        assert!(r.refused, "{text}");
        assert!(
            text.contains("alpha: dependency liby: brew would install 1.2.0"),
            "{text}"
        );
        assert!(!text.contains("dependency libx"), "{text}");
    }

    #[test]
    fn installed_current_dep_passes_alone() {
        let git = core_git(&["alpha", "libx"]);
        let mut by_name = BTreeMap::new();
        by_name.insert("libx".to_string(), Ok(answer(CORE, "1.2.0", false, true)));
        let brew = MockBrew {
            installed: vec![outdated("alpha"), at_cutoff("libx")],
            deps: deps_of(&[("alpha", &["libx"])]),
            by_name,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &["alpha"], &[]);
        assert!(!r.refused, "{text}");
        assert!(run_is_soaked_install(&lock_runs(&brew), "alpha"), "{text}");
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "libx"),
            "an installed dependency brew leaves alone is never installed: {text}"
        );
    }

    #[test]
    fn name_resolving_to_another_tap_holds_the_target() {
        let git = InMemoryGit::new();
        git.insert_blob(
            "tapcut",
            "Formula/cyclonedx-gomod.rb",
            formula_rb("cyclonedx-gomod", "1.1.0", "midsha"),
        );
        git.insert_blob(
            "taphead",
            "Formula/cyclonedx-gomod.rb",
            formula_rb("cyclonedx-gomod", "1.2.0", "newsha"),
        );
        let mut by_name = BTreeMap::new();
        by_name.insert(
            "cyclonedx-gomod".to_string(),
            Ok(answer(CORE, "1.2.0", true, false)),
        );
        let brew = MockBrew {
            installed: vec![formula_pkg_from(
                "cyclonedx-gomod",
                "cyclonedx/cyclonedx",
                formula_rb("cyclonedx-gomod", "1.0.0", "oldsha"),
            )],
            taps: vec![tapped(
                "cyclonedx/cyclonedx",
                Some("https://github.com/cyclonedx/homebrew-cyclonedx"),
            )],
            by_name,
            ..MockBrew::new()
        };
        let snaps = tap_snaps(&git, "cyclonedx/cyclonedx", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Formula/cyclonedx-gomod.rb"]);
        git.insert_tree("taphead", &["Formula/cyclonedx-gomod.rb"]);
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let cache = tempfile::tempdir().expect("cache");
        let tap = tempfile::tempdir().expect("tap");
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
        .expect("upgrade");
        let text = String::from_utf8(out).expect("utf8");
        assert!(r.refused, "{text}");
        assert!(
            text.contains("brew resolves the name to homebrew/core, not cyclonedx/cyclonedx"),
            "{text}"
        );
        assert!(
            text.contains(
                "use `brew upgrade cyclonedx/cyclonedx/cyclonedx-gomod` to bypass brewsoak."
            ),
            "{text}"
        );
        assert!(installs(&brew).is_empty(), "{:?}", installs(&brew));
    }

    #[test]
    fn held_target_takes_back_its_upgraded_count() {
        let git = core_git(&["alpha"]);
        let mut by_name = BTreeMap::new();
        by_name.insert("alpha".to_string(), Ok(answer(CORE, "1.2.0", false, false)));
        let brew = MockBrew {
            installed: vec![outdated("alpha")],
            by_name,
            ..MockBrew::new()
        };
        let (_, text) = run(&brew, &git, &[], &[]);
        assert!(summary(&text, "held", "1"), "{text}");
        assert!(summary(&text, "upgraded", "0"), "{text}");
    }

    #[test]
    fn dry_run_reports_the_same_hold() {
        let git = core_git(&["alpha"]);
        let mut by_name = BTreeMap::new();
        by_name.insert("alpha".to_string(), Ok(answer(CORE, "1.2.0", false, false)));
        let brew = MockBrew {
            installed: vec![outdated("alpha")],
            by_name,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &[], &["--dry-run"]);
        assert!(r.refused, "{text}");
        assert!(
            text.contains("held alpha: brew would install 1.2.0, which is not the soaked cutoff"),
            "{text}"
        );
        assert!(installs(&brew).is_empty(), "{:?}", installs(&brew));
    }

    #[test]
    fn cask_target_is_gated_by_name() {
        let git = InMemoryGit::new();
        git.insert_blob("caskcut", "Casks/a/app.rb", cask_rb("app", "1.1.0"));
        git.insert_blob("caskhead", "Casks/a/app.rb", cask_rb("app", "1.2.0"));
        let ask = |at_cutoff: bool| {
            let mut by_name = BTreeMap::new();
            by_name.insert(
                "app".to_string(),
                Ok(answer("homebrew/cask", "1.2.0", at_cutoff, false)),
            );
            let brew = RecordBrew {
                inner: MockBrew {
                    by_name,
                    ..MockBrew::new()
                },
                tokens: Mutex::new(Vec::new()),
            };
            let cfg = cfg24();
            let inv = inv_from(&brew.inner, &cfg);
            let tap = tempfile::tempdir().expect("tap");
            let mut out = Vec::new();
            let r = install(
                &brew,
                &git,
                &core_snaps(),
                unused_cache(),
                tap.path(),
                &inv,
                &cfg,
                &["app".to_string()],
                true,
                false,
                &[],
                &mut out,
            )
            .expect("install");
            (r, String::from_utf8(out).expect("utf8"), brew)
        };
        let (r, text, brew) = ask(false);
        assert!(r.refused, "{text}");
        let seen = brew.inner.by_name_queries.lock().expect("q").clone();
        assert_eq!(seen[0].kind, PkgKind::Cask);
        assert_eq!(seen[0].name, "app");
        assert!(
            text.contains("held app: brew would install 1.2.0, which is not the soaked cutoff"),
            "{text}"
        );
        assert!(
            text.contains("use `brew install app` to bypass brewsoak."),
            "{text}"
        );
        // Once the gate passes, the dependency list is asked by the bare token.
        let (_, _, brew) = ask(true);
        assert_eq!(brew.tokens.lock().expect("tokens").as_slice(), ["app"]);
    }

    #[test]
    fn by_name_untrusted_tap_refusal_holds_with_the_trust_note() {
        let git = core_git(&["alpha"]);
        let mut by_name = BTreeMap::new();
        by_name.insert(
            "alpha".to_string(),
            Err("Error: alpha: Refusing to load formula acme/tools/alpha from untrusted tap acme/tools.".to_string()),
        );
        let brew = MockBrew {
            installed: vec![outdated("alpha")],
            by_name,
            ..MockBrew::new()
        };
        let (r, text) = run(&brew, &git, &[], &[]);
        assert!(r.refused, "{text}");
        assert!(
            text.contains("brew does not trust tap acme/tools; run brew trust acme/tools"),
            "{text}"
        );
        assert!(summary(&text, "held", "1"), "{text}");
        assert!(summary(&text, "upgraded", "0"), "{text}");
        assert!(
            lock_runs(&brew)
                .iter()
                .all(|a| a.first().map(String::as_str) != Some("trust"))
        );
    }
}
