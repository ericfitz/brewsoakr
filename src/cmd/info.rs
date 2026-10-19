use super::notes::{held_tap_note, unsoakable_note};
use super::view::{
    host_for_pkg, natural_kind, parse_pkg, prefetch_orderings, resolve_token, resolve_view,
    settle_origin,
};
use super::{RunResult, is_verbose};
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::eligibility::DesiredAction;
use crate::git::GitStore;
use crate::inventory::{Inventory, PkgClass};
use crate::nosoak;
use crate::report::{self};
use crate::snapshot::Snapshots;
use std::io::Write;
use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub fn info(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    cfg: &Config,
    names: &[String],
    user_flags: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let mut refused = false;
    let verbose = is_verbose(user_flags);
    let long_form = verbose || !names.is_empty();
    if verbose {
        writeln!(
            out,
            "{}",
            report::soak_banner("showing info", snaps.hours.get(), &snaps.core, &snaps.cask)
        )?;
    }
    if names.is_empty() {
        prefetch_orderings(brew, git, snaps, cache, inv);
    }
    let owned_names: Vec<String> = if names.is_empty() {
        inv.pkgs.iter().map(|p| p.name.clone()).collect()
    } else {
        names.to_vec()
    };
    for (i, raw) in owned_names.iter().enumerate() {
        if long_form && i > 0 {
            writeln!(out)?;
        }
        let mut r = resolve_token(raw, inv, cfg)?;
        let mut kind = None;
        if r.class == PkgClass::Soaked && !snaps.held_taps.contains_key(&r.origin) {
            let k = natural_kind(git, snaps, cache, inv, &r.origin, &r.name)?;
            settle_origin(&mut r, k, inv, cfg);
            kind = Some(k);
        }
        if verbose {
            writeln!(
                out,
                "{}",
                report::origin_line(&r.name, &r.origin, cfg.effective_hours(&r.origin), r.class)
            )?;
        }
        match r.class {
            PkgClass::NoSoak => {
                let installed = inv.find_in(&r.origin, &r.name).and_then(|p| {
                    let host = host_for_pkg(brew, p.kind, &p.name, brew.host_platform().as_ref());
                    parse_pkg(brew, p.kind, &p.receipt_rb, host.as_ref()).ok()
                });
                let inst = installed.as_ref().map(report::identity_version);
                if long_form {
                    writeln!(out, "{raw}")?;
                    writeln!(
                        out,
                        "installed: {}",
                        inst.as_deref().unwrap_or("not installed")
                    )?;
                    writeln!(out, "origin: {}", r.origin)?;
                    writeln!(out, "soak: no-soak (brew decides)")?;
                    writeln!(out, "action: no-soak")?;
                } else {
                    writeln!(
                        out,
                        "{raw}  {}  no-soak (brew)",
                        inst.as_deref().unwrap_or("-")
                    )?;
                }
                continue;
            }
            PkgClass::Unsoakable => {
                if long_form {
                    writeln!(out, "{raw}")?;
                    writeln!(out, "origin: {}", r.origin)?;
                    writeln!(out, "action: unsoakable")?;
                } else {
                    writeln!(out, "{raw}  -  unsoakable")?;
                }
                if !names.is_empty() {
                    let token = nosoak::brew_token(&r.origin, &r.name);
                    writeln!(
                        out,
                        "{}; use `brew info {token}` to bypass brewsoak.",
                        unsoakable_note(&r.name, &r.origin, r.tapped)
                    )?;
                    refused = true;
                }
                continue;
            }
            PkgClass::Soaked => {}
        }
        if let Some(err) = snaps.held_taps.get(&r.origin) {
            writeln!(out, "{raw}")?;
            writeln!(out, "origin: {}", r.origin)?;
            writeln!(out, "{}", held_tap_note(&r.name, &r.origin, err))?;
            continue;
        }
        let kind = kind.expect("soaked packages have a kind");
        let receipt = inv
            .find_in(&r.origin, &r.name)
            .map(|p| p.receipt_rb.as_str());
        let Some(view) = resolve_view(brew, git, snaps, cache, &r.origin, &r.name, kind, receipt)?
        else {
            if long_form {
                writeln!(out, "{raw}")?;
                writeln!(out, "unparseable identity")?;
            } else {
                writeln!(out, "{raw}  -  unparseable identity")?;
            }
            if verbose {
                writeln!(out, "{raw}: unparseable identity; skipped")?;
            }
            continue;
        };
        if verbose {
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    raw,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    view.newer_soaking(),
                    "classified for info",
                )
            )?;
        }
        if long_form {
            writeln!(out, "{raw}")?;
            writeln!(
                out,
                "installed: {}",
                view.installed
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "not installed".into())
            )?;
            writeln!(
                out,
                "cutoff: {}",
                view.cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into())
            )?;
            writeln!(
                out,
                "head: {}",
                view.head
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into())
            )?;
            writeln!(out, "origin: {}", r.origin)?;
            writeln!(out, "soak hours: {}", cfg.effective_hours(&r.origin).get())?;
            let redefined = view.action == DesiredAction::InstallCutoff
                && report::latest_redefined(
                    view.installed
                        .as_ref()
                        .map(report::identity_version)
                        .as_deref(),
                    view.cutoff
                        .as_ref()
                        .map(report::identity_version)
                        .as_deref(),
                );
            if redefined {
                writeln!(out, "action: would reinstall (latest definition changed)")?;
            } else {
                let soaking = view
                    .soaking_label()
                    .map(|label| format!("; {label} soaking"))
                    .unwrap_or_default();
                writeln!(
                    out,
                    "action: {}{soaking}",
                    report::human_action(view.action)
                )?;
            }
            for warn in &view.warnings {
                writeln!(out, "warning: {warn}")?;
            }
        } else {
            let soaking = view
                .soaking_label()
                .map(|label| format!("; {label} soaking"))
                .unwrap_or_default();
            writeln!(
                out,
                "{}{soaking}",
                report::compact_info_line(
                    raw,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.action,
                )
            )?;
        }
    }
    Ok(RunResult {
        refused,
        brew_status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::brew::MockBrew;

    use crate::git::InMemoryGit;

    use crate::cmd::test_support::{
        cask_pkg_from, cask_rb, cfg_with, cfg24, core_snaps, formula_pkg, formula_pkg_from,
        formula_rb, inv_from, local_tap_world, many_detect_world, spawns, tap_cask_world,
        tap_snaps, tapped, unused_cache, view_world,
    };

    #[test]
    fn info_says_a_newer_version_is_soaking() {
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
        // Named: the long form. Bare: one compact line per package.
        for (names, want) in [
            (
                vec!["wget".to_string()],
                "action: up to date (soaked); 1.2.0 soaking",
            ),
            (
                Vec::new(),
                "wget  1.1.0  up to date (soaked); 1.2.0 soaking",
            ),
        ] {
            let mut out = Vec::new();
            info(
                &brew,
                &git,
                &snaps,
                unused_cache(),
                &inv,
                &cfg,
                &names,
                &[],
                &mut out,
            )
            .expect("info");
            let text = String::from_utf8(out).expect("utf8");
            assert!(text.contains(want), "{names:?}: {text}");
        }
    }

    #[test]
    fn info_mentions_cutoff_version_and_install_cutoff() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let names = ["alpha".to_string()];
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = info(
            &MockBrew::new(),
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &names,
            &[],
            &mut out,
        )
        .expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("1.1.0"), "missing cutoff version: {text}");
        assert!(
            text.contains("install cutoff") || text.to_ascii_lowercase().contains("upgrade"),
            "missing install cutoff / upgrade wording: {text}"
        );
        assert!(!result.refused, "info is read-only");
    }

    #[test]
    fn info_says_a_moved_latest_cask_changed_definition() {
        let (brew, git, snaps) = crate::cmd::test_support::greedy_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        info(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["moved".to_string()],
            &["--greedy".to_string()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("action: would reinstall (latest definition changed)"),
            "{text}"
        );
        assert!(!text.contains("would upgrade to :latest"), "{text}");
    }

    #[test]
    fn info_without_names_lists_installed_core() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = info(
            &MockBrew::new(),
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "missing installed alpha: {text}");
        assert!(text.contains("beta"), "missing installed beta: {text}");
        assert!(text.contains("gamma"), "missing installed gamma: {text}");
        assert!(
            !text.contains("action:"),
            "nameless info must be compact: {text}"
        );
        assert!(!result.refused, "info is read-only");
    }

    #[test]
    fn info_names_an_unsoakable_tap_package_and_refuses_it() {
        let (brew, git) = local_tap_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let names = ["alpha".to_string(), "local/tap/thing".to_string()];
        let r = info(
            &MockBrew::new(),
            &git,
            &core_snaps(),
            unused_cache(),
            &inv,
            &cfg,
            &names,
            &[],
            &mut out,
        )
        .expect("info");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "core name still soaked: {text}");
        assert!(text.contains("origin: local/tap"), "{text}");
        assert!(text.contains("unsoakable"), "{text}");
        assert!(text.contains("use `brew info local/tap/thing`"), "{text}");
        assert!(r.refused, "explicitly named unsoakable package is refused");
        assert!(
            brew.visible_runs.lock().expect("visible").is_empty(),
            "info never hands a tap package to brew"
        );
    }

    #[test]
    fn info_bare_name_uses_installed_tap_cask_not_core_formula() {
        let (brew, git, snaps) = tap_cask_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        info(
            &MockBrew::new(),
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["ant".into()],
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("ant: origin anthropics/tap; soak 24h"),
            "{text}"
        );
        assert!(
            text.contains(
                "ant\ninstalled: 1.37.0\ncutoff: 1.38.0\nhead: 1.39.0\norigin: anthropics/tap\nsoak hours: 24\naction: would upgrade"
            ),
            "{text}"
        );
        assert!(!text.contains("homebrew/core"), "{text}");
    }

    #[test]
    fn info_explicit_tap_token_finds_installed_cask_sharing_a_core_formula_name() {
        let git = InMemoryGit::new();
        let core = formula_rb("vacuum", "0.30.6", "s");
        git.insert_blob("cutoffsha", "Formula/v/vacuum.rb", core.clone());
        git.insert_blob("headsha", "Formula/v/vacuum.rb", core.clone());
        git.insert_blob("tapcut", "Casks/vacuum.rb", cask_rb("vacuum", "0.30.6"));
        git.insert_blob("taphead", "Casks/vacuum.rb", cask_rb("vacuum", "0.31.0"));
        let brew = MockBrew {
            installed: vec![
                formula_pkg_from("vacuum", "homebrew/core", core),
                cask_pkg_from(
                    "vacuum",
                    "daveshanley/vacuum",
                    crate::brew::version_only_cask_receipt("vacuum", "0.30.6"),
                ),
            ],
            taps: vec![tapped(
                "daveshanley/vacuum",
                Some("https://github.com/daveshanley/homebrew-vacuum"),
            )],
            ..MockBrew::new()
        };
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let snaps = tap_snaps(&git, "daveshanley/vacuum", Some("tapcut"), "taphead");
        git.insert_tree("tapcut", &["Casks/vacuum.rb"]);
        git.insert_tree("taphead", &["Casks/vacuum.rb"]);
        let mut out = Vec::new();
        info(
            &MockBrew::new(),
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["daveshanley/vacuum/vacuum".into(), "vacuum".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "daveshanley/vacuum/vacuum\ninstalled: 0.30.6\ncutoff: 0.30.6\nhead: 0.31.0\norigin: daveshanley/vacuum\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "vacuum\ninstalled: 0.30.6\ncutoff: 0.30.6\nhead: 0.30.6\norigin: homebrew/core\n"
            ),
            "bare name is the core formula: {text}"
        );
    }

    #[test]
    fn bare_info_detects_and_compares_once_for_every_package() {
        let (brew, git) = many_detect_world(&["app", "bar", "baz"]);
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        info(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("d1.0.0-app"), "{text}");
        assert_eq!(spawns(&brew), (1, 1), "{text}");
    }

    #[test]
    fn info_shows_origin_hours_and_no_soak() {
        let (mut brew, git, snaps) = view_world();
        brew.installed.push(formula_pkg_from(
            "brewsoak",
            "ericfitz/tap",
            formula_rb("brewsoak", "1.0.0", "b"),
        ));
        brew.taps.push(tapped(
            "ericfitz/tap",
            Some("https://github.com/ericfitz/homebrew-tap"),
        ));
        let cfg = cfg_with(
            "NO_SOAK = [\"ericfitz/tap\"]\n[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 48\n",
        );
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        info(
            &MockBrew::new(),
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["alpha".into(), "brewsoak".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("alpha\ninstalled: 1.0.0\ncutoff: 1.1.0\nhead: 1.2.0\norigin: homebrew/core\nsoak hours: 48\naction: would upgrade"),
            "{text}"
        );
        assert!(
            text.contains("brewsoak\ninstalled: 1.0.0\n")
                && text.contains(
                    "origin: ericfitz/tap\nsoak: no-soak (brew decides)\naction: no-soak"
                ),
            "{text}"
        );
    }
}
