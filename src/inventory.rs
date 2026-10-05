//! Every installed package with its origin tap and soak class.

use crate::Error;
use crate::brew::{Brew, InstalledPkg};
use crate::config::Config;
use crate::origin::{self, OriginRecords};
use crate::resolve::PkgKind;
use crate::taps::{self, TapClass, TapInfo};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgClass {
    Soaked,
    NoSoak,
    Unsoakable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkg {
    pub name: String,
    pub kind: PkgKind,
    /// `user/repo`, lowercase; core/cask included.
    pub origin: String,
    pub class: PkgClass,
    pub receipt_rb: String,
    /// What brew's receipt says; `None` when it was staged by brewsoak.
    pub receipt_tap: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub pkgs: Vec<Pkg>,
    pub taps: BTreeMap<String, TapClass>,
    pub tap_remotes: BTreeMap<String, String>,
}

impl Inventory {
    pub fn build(
        installed: Vec<InstalledPkg>,
        taps: &[TapInfo],
        origins: &OriginRecords,
        cfg: &Config,
    ) -> Self {
        let mut inv = Inventory::default();
        for tap in taps {
            let name = tap.name.to_ascii_lowercase();
            inv.taps.insert(name.clone(), taps::classify(tap));
            if let Some(remote) = &tap.remote {
                inv.tap_remotes.insert(name, remote.clone());
            }
        }
        for p in installed {
            let origin = origin::resolve_origin(p.tap.as_deref(), origins, p.kind, &p.name);
            let class = inv.class_for(&origin, &p.name, cfg);
            inv.pkgs.push(Pkg {
                name: p.name,
                kind: p.kind,
                origin,
                class,
                receipt_rb: p.receipt_rb,
                receipt_tap: p.tap,
                pinned: p.pinned,
            });
        }
        inv
    }

    pub fn load(brew: &impl Brew, cache: &Path, cfg: &Config) -> Result<Self, Error> {
        let installed = brew.installed_packages()?;
        let taps = brew.tap_info()?;
        let origins = OriginRecords::load(cache);
        Ok(Self::build(installed, &taps, &origins, cfg))
    }

    /// Bare-name lookup. When a formula and a cask share a name, the
    /// formula wins, as `brew` does for a bare token.
    pub fn find(&self, name: &str) -> Option<&Pkg> {
        self.pkgs
            .iter()
            .find(|p| p.name == name && p.kind == PkgKind::Formula)
            .or_else(|| self.pkgs.iter().find(|p| p.name == name))
    }

    /// The installed package with this name from exactly this origin tap.
    pub fn find_in(&self, origin: &str, name: &str) -> Option<&Pkg> {
        self.pkgs
            .iter()
            .find(|p| p.name == name && p.origin.eq_ignore_ascii_case(origin))
    }

    pub fn tap_class(&self, tap: &str) -> Option<TapClass> {
        self.taps.get(&tap.to_ascii_lowercase()).copied()
    }

    /// NO_SOAK first; then core/cask and soakable taps are soaked; anything
    /// else (no remote, non-HTTPS, staging, or not tapped at all) is unsoakable.
    pub fn class_for(&self, origin_tap: &str, name: &str, cfg: &Config) -> PkgClass {
        let origin_tap = origin_tap.to_ascii_lowercase();
        let origin_tap = origin_tap.as_str();
        if cfg.is_no_soak(origin_tap, name) {
            return PkgClass::NoSoak;
        }
        if origin::is_core_or_cask(origin_tap) {
            return PkgClass::Soaked;
        }
        match self.tap_class(origin_tap) {
            Some(TapClass::Soakable) => PkgClass::Soaked,
            _ => PkgClass::Unsoakable,
        }
    }

    /// Soakable taps with at least one installed soaked package, plus the
    /// origin of each explicit `(origin, name)` token that is itself soaked
    /// (not no-soak, not unsoakable). Names are lowercase.
    pub fn needed_taps(&self, extra: &[(String, String)], cfg: &Config) -> Vec<(String, String)> {
        let mut names: Vec<String> = self
            .pkgs
            .iter()
            .filter(|p| p.class == PkgClass::Soaked && !origin::is_core_or_cask(&p.origin))
            .map(|p| p.origin.clone())
            .chain(
                extra
                    .iter()
                    .filter(|(o, n)| self.class_for(o, n, cfg) == PkgClass::Soaked)
                    .map(|(o, _)| o.to_ascii_lowercase()),
            )
            .filter(|t| self.tap_class(t) == Some(TapClass::Soakable))
            .collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter_map(|n| self.tap_remotes.get(&n).map(|r| (n.clone(), r.clone())))
            .collect()
    }

    pub fn any_no_soak(&self) -> bool {
        self.pkgs.iter().any(|p| p.class == PkgClass::NoSoak)
    }
}

/// An explicit command-line name: `name` or `user/repo/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub origin: Option<String>,
    pub name: String,
}

pub fn parse_token(raw: &str) -> Result<Token, Error> {
    match raw.matches('/').count() {
        0 => Ok(Token {
            origin: None,
            name: raw.to_string(),
        }),
        2 => {
            let (tap, name) = raw.rsplit_once('/').expect("two slashes");
            if origin::split_tap(tap).is_none() || name.is_empty() {
                return Err(Error::Usage(format!("{raw}: expected user/repo/name")));
            }
            Ok(Token {
                origin: Some(tap.to_ascii_lowercase()),
                name: name.to_string(),
            })
        }
        _ => Err(Error::Usage(format!(
            "{raw}: expected name or user/repo/name"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SoakHours;
    use crate::config::parse_file;

    fn cfg(no_soak: &str) -> Config {
        let parsed = parse_file(&format!("NO_SOAK = {no_soak}\n"));
        Config {
            no_soak: parsed.no_soak,
            ..Config::uniform(SoakHours::new(24).unwrap())
        }
    }

    fn installed(name: &str, kind: PkgKind, tap: Option<&str>) -> InstalledPkg {
        InstalledPkg {
            name: name.into(),
            kind,
            receipt_rb: "rb".into(),
            pinned: false,
            tap: tap.map(str::to_string),
        }
    }

    fn taps() -> Vec<TapInfo> {
        vec![
            TapInfo {
                name: "homebrew/core".into(),
                remote: None,
            },
            TapInfo {
                name: "homebrew/cask".into(),
                remote: None,
            },
            TapInfo {
                name: "brewsoakr/soaked".into(),
                remote: None,
            },
            TapInfo {
                name: "hashicorp/tap".into(),
                remote: Some("https://github.com/hashicorp/homebrew-tap".into()),
            },
            TapInfo {
                name: "ericfitz/tap".into(),
                remote: Some("https://github.com/ericfitz/homebrew-tap".into()),
            },
            TapInfo {
                name: "local/tap".into(),
                remote: None,
            },
        ]
    }

    #[test]
    fn build_classifies_every_installed_package() {
        let mut origins = OriginRecords::default();
        origins.set(PkgKind::Formula, "vault", "hashicorp/tap");
        let inv = Inventory::build(
            vec![
                installed("wget", PkgKind::Formula, Some("homebrew/core")),
                installed("ca-certificates", PkgKind::Formula, None),
                installed("terraform", PkgKind::Formula, Some("hashicorp/tap")),
                installed("vault", PkgKind::Formula, None),
                installed("brewsoak", PkgKind::Formula, Some("ericfitz/tap")),
                installed("thing", PkgKind::Formula, Some("local/tap")),
                installed("firefox", PkgKind::Cask, Some("homebrew/cask")),
            ],
            &taps(),
            &origins,
            &cfg("[\"ericfitz/tap\", \"wget\"]"),
        );
        let by = |n: &str| inv.find(n).unwrap();
        assert_eq!(
            (by("wget").origin.as_str(), by("wget").class),
            ("homebrew/core", PkgClass::NoSoak)
        );
        assert_eq!(
            (
                by("ca-certificates").origin.as_str(),
                by("ca-certificates").class
            ),
            ("homebrew/core", PkgClass::Soaked)
        );
        assert_eq!(
            (by("terraform").origin.as_str(), by("terraform").class),
            ("hashicorp/tap", PkgClass::Soaked)
        );
        assert_eq!(
            (by("vault").origin.as_str(), by("vault").class),
            ("hashicorp/tap", PkgClass::Soaked),
            "staged install: origin from record"
        );
        assert_eq!(by("vault").receipt_tap, None);
        assert_eq!(
            (by("brewsoak").origin.as_str(), by("brewsoak").class),
            ("ericfitz/tap", PkgClass::NoSoak)
        );
        assert_eq!(
            (by("thing").origin.as_str(), by("thing").class),
            ("local/tap", PkgClass::Unsoakable)
        );
        assert_eq!(by("firefox").class, PkgClass::Soaked);
        assert_eq!(inv.tap_class("hashicorp/tap"), Some(TapClass::Soakable));
        assert_eq!(inv.tap_class("brewsoakr/soaked"), Some(TapClass::Staging));
        assert!(inv.any_no_soak());
    }

    #[test]
    fn no_soak_wins_over_unsoakable() {
        let inv = Inventory::build(
            vec![installed("thing", PkgKind::Formula, Some("local/tap"))],
            &taps(),
            &OriginRecords::default(),
            &cfg("[\"local/tap\"]"),
        );
        assert_eq!(inv.find("thing").unwrap().class, PkgClass::NoSoak);
    }

    #[test]
    fn receipt_tap_not_installed_anymore_is_unsoakable() {
        let inv = Inventory::build(
            vec![installed("gone", PkgKind::Formula, Some("old/tap"))],
            &taps(),
            &OriginRecords::default(),
            &cfg("[]"),
        );
        assert_eq!(inv.find("gone").unwrap().class, PkgClass::Unsoakable);
        assert_eq!(
            inv.class_for("old/tap", "gone", &cfg("[]")),
            PkgClass::Unsoakable
        );
        assert_eq!(
            inv.class_for("old/tap", "gone", &cfg("[\"old/tap\"]")),
            PkgClass::NoSoak
        );
    }

    #[test]
    fn needed_taps_are_soakable_with_a_soaked_package_plus_extras() {
        let inv = Inventory::build(
            vec![
                installed("terraform", PkgKind::Formula, Some("hashicorp/tap")),
                installed("brewsoak", PkgKind::Formula, Some("ericfitz/tap")),
                installed("thing", PkgKind::Formula, Some("local/tap")),
            ],
            &taps(),
            &OriginRecords::default(),
            &cfg("[\"ericfitz/tap\"]"),
        );
        let c0 = cfg("[\"ericfitz/tap\"]");
        assert_eq!(
            inv.needed_taps(&[], &c0),
            vec![(
                "hashicorp/tap".to_string(),
                "https://github.com/hashicorp/homebrew-tap".to_string()
            )]
        );
        let c = cfg("[\"ericfitz/tap\"]");
        let t = |o: &str, n: &str| (o.to_string(), n.to_string());
        let with_extra = inv.needed_taps(
            &[
                t("ericfitz/tap", "other"),
                t("local/tap", "x"),
                t("homebrew/core", "wget"),
            ],
            &c,
        );
        assert_eq!(
            with_extra.len(),
            1,
            "no-soak, unsoakable, core extras add nothing: {with_extra:?}"
        );
        let c2 = cfg("[\"ericfitz/tap/skipme\"]");
        let extra = inv.needed_taps(
            &[t("ericfitz/tap", "other"), t("ericfitz/tap", "skipme")],
            &c2,
        );
        assert!(
            extra.iter().any(|(n, _)| n == "ericfitz/tap"),
            "soaked token pulls its tap: {extra:?}"
        );
        let only_nosoak = inv.needed_taps(&[t("ericfitz/tap", "skipme")], &c2);
        assert!(
            !only_nosoak.iter().any(|(n, _)| n == "ericfitz/tap"),
            "{only_nosoak:?}"
        );
    }

    #[test]
    fn class_for_not_installed_names() {
        let inv = Inventory::build(
            Vec::new(),
            &taps(),
            &OriginRecords::default(),
            &cfg("[\"wget\"]"),
        );
        let c = cfg("[\"wget\"]");
        assert_eq!(inv.class_for("homebrew/core", "wget", &c), PkgClass::NoSoak);
        assert_eq!(inv.class_for("homebrew/core", "curl", &c), PkgClass::Soaked);
        assert_eq!(
            inv.class_for("hashicorp/tap", "vault", &c),
            PkgClass::Soaked
        );
        assert_eq!(inv.class_for("local/tap", "x", &c), PkgClass::Unsoakable);
        assert_eq!(
            inv.class_for("nobody/tap", "x", &c),
            PkgClass::Unsoakable,
            "untapped tap"
        );
    }

    #[test]
    fn parse_token_forms() {
        assert_eq!(
            parse_token("wget").unwrap(),
            Token {
                origin: None,
                name: "wget".into()
            }
        );
        assert_eq!(
            parse_token("HashiCorp/tap/terraform").unwrap(),
            Token {
                origin: Some("hashicorp/tap".into()),
                name: "terraform".into()
            }
        );
        assert!(matches!(parse_token("user/foo"), Err(Error::Usage(_))));
        assert!(matches!(parse_token("a/b/c/d"), Err(Error::Usage(_))));
    }

    #[test]
    fn third_party_cask_is_soaked_from_its_tap_and_find_in_separates_same_names() {
        let mut t = taps();
        t.push(TapInfo {
            name: "anthropics/tap".into(),
            remote: Some("https://github.com/anthropics/homebrew-tap".into()),
        });
        t.push(TapInfo {
            name: "daveshanley/vacuum".into(),
            remote: Some("https://github.com/daveshanley/homebrew-vacuum".into()),
        });
        let inv = Inventory::build(
            vec![
                installed("vacuum", PkgKind::Formula, Some("homebrew/core")),
                installed("vacuum", PkgKind::Cask, Some("daveshanley/vacuum")),
                installed("ant", PkgKind::Cask, Some("anthropics/tap")),
            ],
            &t,
            &OriginRecords::default(),
            &cfg("[\"ericfitz/tap\"]"),
        );
        let ant = inv.find("ant").unwrap();
        assert_eq!(
            (ant.kind, ant.origin.as_str(), ant.class),
            (PkgKind::Cask, "anthropics/tap", PkgClass::Soaked)
        );
        assert_eq!(
            inv.find("vacuum").unwrap().kind,
            PkgKind::Formula,
            "bare name: formula first, as brew does"
        );
        assert_eq!(
            inv.find_in("daveshanley/vacuum", "vacuum").unwrap().kind,
            PkgKind::Cask
        );
        assert_eq!(
            inv.find_in("homebrew/core", "vacuum").unwrap().kind,
            PkgKind::Formula
        );
        assert!(inv.find_in("homebrew/cask", "vacuum").is_none());
        assert!(inv.find_in("DaveShanley/Vacuum", "vacuum").is_some());
        let needed: Vec<String> = inv
            .needed_taps(&[], &cfg("[\"ericfitz/tap\"]"))
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(needed, vec!["anthropics/tap", "daveshanley/vacuum"]);
    }

    #[test]
    fn mixed_case_receipt_and_tap_names_are_lowercased() {
        let mut t = taps();
        t.push(TapInfo {
            name: "HashiCorp/Other".into(),
            remote: Some("https://github.com/h/o".into()),
        });
        let inv = Inventory::build(
            vec![installed("x", PkgKind::Formula, Some("HashiCorp/Other"))],
            &t,
            &OriginRecords::default(),
            &cfg("[]"),
        );
        assert_eq!(inv.find("x").unwrap().origin, "hashicorp/other");
        assert_eq!(inv.find("x").unwrap().class, PkgClass::Soaked);
        assert_eq!(
            inv.class_for("HashiCorp/Other", "x", &cfg("[]")),
            PkgClass::Soaked
        );
        assert_eq!(inv.needed_taps(&[], &cfg("[]"))[0].0, "hashicorp/other");
    }
}
