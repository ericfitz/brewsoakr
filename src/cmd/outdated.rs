use super::bare::{bare_action, latest_left, latest_note};
use super::notes::{held_tap_note, hold_why, unsoakable_note, write_section, write_section_always};
use super::view::{prefetch_orderings, resolve_view, view_versions};
use super::{RunResult, is_verbose};
use crate::Error;
use crate::brew::Brew;
use crate::config::Config;
use crate::eligibility::DesiredAction;
use crate::flags;
use crate::git::GitStore;
use crate::inventory::{Inventory, PkgClass};
use crate::report::{self};
use crate::snapshot::Snapshots;
use std::io::Write;
use std::path::Path;

/// `name (installed) < cutoff`. A `:latest` cask has no version to order, and
/// is listed only because its cutoff definition (url or sha) differs from the
/// installed one, so say that in brew's own `outdated --verbose` style
/// (`Cask#outdated_info`: `token (latest) != latest`, without the colon).
fn compare_line(name: &str, installed: &str, cmp: char, cutoff: &str) -> String {
    if crate::report::latest_redefined(Some(installed), Some(cutoff)) {
        return format!("{name} (latest) != latest (definition changed)");
    }
    format!("{name} ({installed}) {cmp} {cutoff}")
}

#[allow(clippy::too_many_arguments)]
pub fn outdated(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    cfg: &Config,
    extra_args: &[String],
    out: &mut impl Write,
) -> Result<RunResult, Error> {
    let verbose = is_verbose(extra_args);
    let greedy = flags::greedy_mode(extra_args);
    if verbose {
        writeln!(
            out,
            "{}",
            report::soak_banner(
                "checking outdated",
                snaps.hours.get(),
                &snaps.core,
                &snaps.cask
            )
        )?;
    }
    let mut upgrades = Vec::new();
    let mut held = Vec::new();
    let mut ahead = Vec::new();
    let mut auto_updates = Vec::new();
    let mut pinned = Vec::new();
    let mut soaking = Vec::new();
    let mut soaked = 0usize;
    let mut no_soak_names = Vec::new();
    prefetch_orderings(brew, git, snaps, cache, inv);
    for pkg in &inv.pkgs {
        if verbose {
            writeln!(
                out,
                "{}",
                report::origin_line(
                    &pkg.name,
                    &pkg.origin,
                    cfg.effective_hours(&pkg.origin),
                    pkg.class
                )
            )?;
        }
        match pkg.class {
            PkgClass::NoSoak => {
                if pkg.pinned {
                    pinned.push(pkg.name.clone());
                } else {
                    no_soak_names.push(pkg.name.clone());
                }
                if verbose {
                    writeln!(out, "{}: no-soak; brew decides", pkg.name)?;
                }
                continue;
            }
            PkgClass::Unsoakable => {
                held.push(unsoakable_note(
                    &pkg.name,
                    &pkg.origin,
                    inv.tap_class(&pkg.origin).is_some(),
                ));
                continue;
            }
            PkgClass::Soaked => {}
        }
        if pkg.pinned {
            pinned.push(pkg.name.clone());
            if verbose {
                writeln!(out, "{}: pinned; skipped", pkg.name)?;
            }
            continue;
        }
        if let Some(err) = snaps.held_taps.get(&pkg.origin) {
            held.push(held_tap_note(&pkg.name, &pkg.origin, err));
            continue;
        }
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
            held.push(format!("{}: unparseable identity", pkg.name));
            if verbose {
                writeln!(out, "{}: unparseable identity; skipped", pkg.name)?;
            }
            continue;
        };
        // `bare_action` may turn the action into `LeaveAutoUpdates`; keep the
        // original so that entry prints the right comparison.
        let resolved_action = view.action;
        view.action = bare_action(&view, greedy);
        for warn in &view.warnings {
            writeln!(out, "warning: {warn}")?;
        }
        if verbose {
            writeln!(
                out,
                "{}",
                report::evaluate_line(
                    &pkg.name,
                    view.action,
                    view.installed.as_ref(),
                    view.cutoff.as_ref(),
                    view.head.as_ref(),
                    view.newer_soaking(),
                    "classified for outdated",
                )
            )?;
            if latest_left(&view, greedy) {
                writeln!(out, "{}", latest_note(&pkg.name))?;
            }
        }
        match view.action {
            DesiredAction::InstallCutoff => {
                let installed_ver = view
                    .installed
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "unknown".into());
                let cutoff_ver = view
                    .cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into());
                upgrades.push(compare_line(&pkg.name, &installed_ver, '<', &cutoff_ver));
            }
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated
            | DesiredAction::HoldUnordered => {
                if let Some(why) = hold_why(view.action) {
                    held.push(format!("{}: {why}", pkg.name));
                }
            }
            DesiredAction::LeaveAheadOfSoak => ahead.push(pkg.name.clone()),
            DesiredAction::LeaveNewerThanCutoff => {
                let (installed_ver, cutoff_ver) = view_versions(&view);
                ahead.push(format!("{} ({installed_ver}) > {cutoff_ver}", pkg.name));
            }
            DesiredAction::LeaveAutoUpdates => {
                let installed_ver = view
                    .installed
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "unknown".into());
                let cutoff_ver = view
                    .cutoff
                    .as_ref()
                    .map(report::identity_version)
                    .unwrap_or_else(|| "none".into());
                let cmp = match resolved_action {
                    DesiredAction::LeaveNewerThanCutoff => '>',
                    DesiredAction::HoldUnordered => '?',
                    _ => '<',
                };
                auto_updates.push(compare_line(&pkg.name, &installed_ver, cmp, &cutoff_ver));
            }
            DesiredAction::NoOpAlreadySoaked => match view.soaking_label() {
                Some(label) => {
                    let (installed_ver, _) = view_versions(&view);
                    soaking.push(format!("{} ({installed_ver}) -> {label}", pkg.name));
                }
                None => soaked += 1,
            },
        }
    }
    let brew_outdated = if no_soak_names.is_empty() {
        Vec::new()
    } else {
        brew.outdated_names(greedy)?
    };
    let no_soak: Vec<String> = no_soak_names
        .iter()
        .filter(|n| brew_outdated.contains(n))
        .map(|n| format!("{n} (no-soak, brew)"))
        .collect();
    write_section_always(out, "==> Outdated (will upgrade)", &upgrades)?;
    write_section_always(out, "==> No-soak (brew)", &no_soak)?;
    write_section_always(out, "==> Held", &held)?;
    write_section(
        out,
        "==> Soaking (installed is the newest soaked version)",
        &soaking,
    )?;
    write_section_always(out, "==> Ahead of soak", &ahead)?;
    write_section_always(
        out,
        "==> Auto-updates (left to the app; name it to upgrade)",
        &auto_updates,
    )?;
    write_section_always(out, "==> Pinned", &pinned)?;
    if upgrades.is_empty()
        && no_soak.is_empty()
        && held.is_empty()
        && ahead.is_empty()
        && auto_updates.is_empty()
        && pinned.is_empty()
    {
        writeln!(out, "nothing outdated (already soaked: {soaked})")?;
    }
    Ok(RunResult {
        refused: false,
        brew_status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::brew::MockBrew;

    use crate::git::InMemoryGit;

    use crate::cmd::bare::latest_note;

    use crate::cmd::test_support::{
        AUTO_UPDATES_HEADER, OUTDATED_HEADER, auto_updates_world, cfg_with, cfg24, core_snaps,
        formula_pkg, formula_pkg_from, formula_pkg_pinned, formula_rb, greedy_outdated,
        greedy_world, inv_from, local_tap_world, lock_runs, openssl_is_newer,
        openssl_repoint_world, run_has_token, section_lines, tapped, unused_cache, view_world,
    };
    use crate::cmd::view::calendar_today;

    #[test]
    fn outdated_lists_upgrade_held_and_ahead_sections() {
        let (brew, git, snaps) = view_world();
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
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
    fn outdated_warns_future_deprecate_and_does_not_hold() {
        let old = formula_rb("py", "3.13.0", "oldsha");
        let mid = formula_rb("py", "3.14.0", "midsha");
        // Inside the one-year warning horizon whatever day the test runs:
        // later than today, no later than today + 1 year.
        let year: u32 = calendar_today()[..4].parse().unwrap();
        let soon = format!("{}-01-01", year + 1);
        let new = format!(
            "{}\n  deprecate! date: \"{soon}\", because: :deprecated_upstream\n",
            formula_rb("py", "3.14.1", "newsha").trim_end()
        );
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/p/py.rb", mid);
        git.insert_blob("headsha", "Formula/p/py.rb", new);
        let brew = MockBrew {
            installed: vec![formula_pkg("py", old)],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(&format!("warning: py scheduled to be deprecated on {soon}")),
            "{text}"
        );
        assert!(text.contains("==> Outdated"), "{text}");
        assert!(
            !text.contains("py:") || !text.contains("Held"),
            "future deprecate must not hold: {text}"
        );
    }

    #[test]
    fn outdated_skips_unparseable_identity_and_continues() {
        let alpha_old = formula_rb("alpha", "1.0.0", "oldsha");
        let alpha_mid = formula_rb("alpha", "1.1.0", "midsha");
        let alpha_new = formula_rb("alpha", "1.2.0", "newsha");
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/a/alpha.rb", alpha_mid);
        git.insert_blob("headsha", "Formula/a/alpha.rb", alpha_new);
        git.insert_blob("cutoffsha", "Formula/b/bad.rb", "class Bad; end\n");
        git.insert_blob("headsha", "Formula/b/bad.rb", "class Bad; end\n");

        let brew = MockBrew {
            installed: vec![
                formula_pkg("bad", "class Bad; end\n".into()),
                formula_pkg("alpha", alpha_old),
            ],
            ..MockBrew::new()
        };
        let snaps = core_snaps();
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let result = outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "good pkg must still list: {text}");
        assert!(
            text.contains("bad") && text.contains("unparseable"),
            "unparseable pkg must be held, not abort: {text}"
        );
        assert!(!result.refused);
    }

    #[test]
    fn outdated_lists_a_package_whose_newer_version_is_soaking() {
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
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "==> Soaking (installed is the newest soaked version)\nwget (1.1.0) -> 1.2.0\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("nothing outdated (already soaked: 0)"),
            "{text}"
        );
    }

    #[test]
    fn outdated_skips_pinned_formula() {
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
        let mut out = Vec::new();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("ok"), "unpinned must list: {text}");
        assert!(
            text.contains("==> Pinned"),
            "missing pinned section: {text}"
        );
        let outdated_block = text.split("==> Pinned").next().unwrap_or(&text);
        assert!(
            !outdated_block
                .split("==> Held")
                .next()
                .unwrap_or("")
                .contains("pin"),
            "pinned must not list as outdated: {text}"
        );
    }

    #[test]
    fn outdated_notes_unsoakable_packages_without_brew() {
        let (brew, git) = local_tap_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        let r = outdated(
            &brew,
            &git,
            &core_snaps(),
            unused_cache(),
            &inv,
            &cfg,
            &["acme/tools/foo".to_string()],
            &mut out,
        )
        .expect("outdated");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("alpha"), "soaked listing still runs: {text}");
        assert!(
            text.contains(
                "thing: tap local/tap has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK"
            ),
            "{text}"
        );
        assert!(!r.refused);
        assert!(brew.visible_runs.lock().expect("visible").is_empty());
    }

    #[test]
    fn outdated_verbose_says_a_moved_latest_cask_changed_definition() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["--greedy", "-v"]);
        assert!(
            text.contains("moved: installed :latest definition differs from cutoff; reinstalling"),
            "{text}"
        );
        assert!(!text.contains("moved: installing cutoff :latest"), "{text}");
    }

    #[test]
    fn outdated_greedy_lists_self_updating_casks_as_outdated() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["--greedy"]);
        let upgrades = section_lines(&text, OUTDATED_HEADER);
        assert!(
            upgrades.contains(&"alt-tab (7.38.1) < 11.8.0".to_string()),
            "{text}"
        );
        assert!(
            upgrades.contains(&"self-latest (latest) != latest (definition changed)".to_string()),
            "{text}"
        );
        assert!(
            upgrades.contains(&"moved (latest) != latest (definition changed)".to_string()),
            "{text}"
        );
        assert!(
            upgrades.contains(&"wget (1.0.0) < 1.1.0".to_string()),
            "{text}"
        );
        assert_eq!(upgrades.len(), 4, "{text}");
        assert!(
            section_lines(&text, AUTO_UPDATES_HEADER).is_empty(),
            "{text}"
        );
        assert!(
            !text.contains("cannot be soaked"),
            "the note needs -v: {text}"
        );
    }

    #[test]
    fn outdated_greedy_latest_notes_latest_casks_with_verbose() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["--greedy-latest", "-v"]);
        assert!(text.contains(&latest_note("nightly")), "{text}");
        assert_eq!(
            text.matches("cannot be soaked").count(),
            1,
            "only nightly: {text}"
        );
        let auto = section_lines(&text, AUTO_UPDATES_HEADER);
        assert_eq!(
            auto.len(),
            2,
            "alt-tab and self-latest stay left to the app: {text}"
        );
        assert_eq!(section_lines(&text, OUTDATED_HEADER).len(), 2, "{text}");
    }

    #[test]
    fn outdated_greedy_auto_updates_lists_self_updating_and_skips_the_note() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["--greedy-auto-updates", "-v"]);
        assert_eq!(section_lines(&text, OUTDATED_HEADER).len(), 4, "{text}");
        assert!(
            section_lines(&text, AUTO_UPDATES_HEADER).is_empty(),
            "{text}"
        );
        assert!(!text.contains("cannot be soaked"), "{text}");
    }

    #[test]
    fn outdated_short_g_is_greedy() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_outdated(&brew, &git, &snaps, &["-g"]);
        assert_eq!(section_lines(&text, OUTDATED_HEADER).len(), 4, "{text}");
        let text = greedy_outdated(&brew, &git, &snaps, &["-gv"]);
        assert_eq!(section_lines(&text, OUTDATED_HEADER).len(), 4, "{text}");
        assert!(
            text.contains("classified for outdated"),
            "-gv is verbose: {text}"
        );
        let text = greedy_outdated(&brew, &git, &snaps, &["-v", "--greedy-latest"]);
        assert!(text.contains(&latest_note("nightly")), "{text}");
    }

    #[test]
    fn outdated_lists_a_repointed_formula_as_ahead_not_outdated() {
        let (mut brew, git, snaps) = openssl_repoint_world();
        openssl_is_newer(&mut brew);
        let text = greedy_outdated(&brew, &git, &snaps, &[]);
        assert!(section_lines(&text, OUTDATED_HEADER).is_empty(), "{text}");
        let ahead = section_lines(&text, "==> Ahead of soak");
        assert_eq!(ahead, ["openssl@3 (3.6.5) > 3.5.9"], "{text}");
    }

    #[test]
    fn outdated_lists_auto_updates_cask_in_its_own_section() {
        let (brew, git, snaps) = auto_updates_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("==> Outdated (will upgrade)\n(none)\n"),
            "brew does not list auto_updates casks without --greedy: {text}"
        );
        assert!(
            text.contains("==> Auto-updates (left to the app; name it to upgrade)\nalt-tab (7.38.1) < 11.8.0\n"),
            "{text}"
        );
        assert!(
            !text.contains("nothing outdated"),
            "a non-empty Auto-updates section is something outdated: {text}"
        );
    }

    #[test]
    fn verbose_prints_origin_and_hours_per_package() {
        let (brew, git, snaps) = view_world();
        let cfg = cfg_with("[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 48\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("alpha: origin homebrew/core; soak 48h"),
            "{text}"
        );
    }

    #[test]
    fn outdated_lists_no_soak_from_brews_view_without_updating() {
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
        brew.outdated = vec!["brewsoak".into(), "alpha".into()];
        let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &git,
            &snaps,
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("==> No-soak (brew)\nbrewsoak (no-soak, brew)"),
            "{text}"
        );
        assert!(
            !text.contains("alpha (no-soak"),
            "soaked packages are not listed from brew's view: {text}"
        );
        assert!(!run_has_token(&lock_runs(&brew), "update"));
    }

    #[test]
    fn no_soak_outdated_honors_the_greedy_flags_like_upgrade() {
        for (flags, want) in [
            (vec![], flags::Greedy::default()),
            (
                vec!["--greedy".to_string()],
                flags::Greedy {
                    auto_updates: true,
                    latest: true,
                },
            ),
            (
                vec!["-g".to_string()],
                flags::Greedy {
                    auto_updates: true,
                    latest: true,
                },
            ),
            (
                vec!["--greedy-latest".to_string()],
                flags::Greedy {
                    auto_updates: false,
                    latest: true,
                },
            ),
            (
                vec!["--greedy-auto-updates".to_string()],
                flags::Greedy {
                    auto_updates: true,
                    latest: false,
                },
            ),
        ] {
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
            let cfg = cfg_with("NO_SOAK = [\"ericfitz/tap\"]\n");
            let inv = inv_from(&brew, &cfg);
            let mut out = Vec::new();
            outdated(
                &brew,
                &git,
                &snaps,
                unused_cache(),
                &inv,
                &cfg,
                &flags,
                &mut out,
            )
            .unwrap();
            assert_eq!(
                brew.outdated_modes.lock().unwrap().as_slice(),
                [want],
                "{flags:?}"
            );
        }
    }

    #[test]
    fn outdated_lists_a_pinned_no_soak_package_under_pinned() {
        let brew = MockBrew {
            installed: vec![formula_pkg_pinned("wget", formula_rb("wget", "1.0.0", "a"))],
            outdated: vec!["wget".into()],
            ..MockBrew::new()
        };
        let cfg = cfg_with("NO_SOAK = [\"wget\"]\n");
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        outdated(
            &brew,
            &InMemoryGit::new(),
            &core_snaps(),
            unused_cache(),
            &inv,
            &cfg,
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("==> Pinned\nwget"), "{text}");
        assert!(!text.contains("wget (no-soak"), "{text}");
    }
}
