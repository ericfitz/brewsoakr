use super::view::{ResolvedView, prefetch_orderings, resolve_view};
use crate::brew::Brew;
use crate::eligibility::DesiredAction;
use crate::flags;
use crate::git::GitStore;
use crate::inventory::{Inventory, Pkg, PkgClass};
use crate::origin::{self};
use crate::snapshot::Snapshots;
use std::path::Path;

/// Installed soaked packages whose origin has a snapshot in this run. A tap
/// that failed to refresh (even if it keeps its last good table) or has no
/// state contributes nothing.
pub(super) fn snapshotted_pkgs<'a>(
    inv: &'a Inventory,
    snaps: &'a Snapshots,
) -> impl Iterator<Item = &'a Pkg> {
    inv.pkgs.iter().filter(|p| {
        p.class == PkgClass::Soaked
            && !snaps.held_taps.contains_key(&p.origin)
            && (origin::is_core_or_cask(&p.origin) || snaps.tap(&p.origin).is_some())
    })
}

/// Why a bare run does not evaluate an installed package, in the order the
/// run checks. `plan_size` and the bare loop both read this, so the `[i/N]`
/// total cannot drift from what the loop announces.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BareSkip<'a> {
    Pinned,
    /// No-soak or unsoakable: brew's, or a note.
    NotSoaked,
    /// Its tap could not be refreshed this run; carries git's error.
    HeldTap(&'a str),
}

pub(super) fn bare_skip<'a>(pkg: &Pkg, snaps: &'a Snapshots) -> Option<BareSkip<'a>> {
    if pkg.pinned {
        Some(BareSkip::Pinned)
    } else if pkg.class != PkgClass::Soaked {
        Some(BareSkip::NotSoaked)
    } else {
        snaps
            .held_taps
            .get(&pkg.origin)
            .map(|err| BareSkip::HeldTap(err))
    }
}

/// How many installed packages the soak window says to change. Resolution is
/// cheap (cached blobs) and errors just mean "no total to show".
pub(super) fn plan_size(
    brew: &impl Brew,
    git: &impl GitStore,
    snaps: &Snapshots,
    cache: &Path,
    inv: &Inventory,
    greedy: flags::Greedy,
) -> usize {
    // Keep this in step with `apply_resolved` and the `outdated` report:
    // every package counted here must reach `announce` there and land under
    // "Outdated (will upgrade)", and none other. All three go through
    // `bare_action` with the same greedy mode; the test
    // `plan_size_upgrade_and_outdated_agree_under_every_greedy_mode` pins that
    // under all four modes.
    prefetch_orderings(brew, git, snaps, cache, inv);
    snapshotted_pkgs(inv, snaps)
        .filter(|pkg| bare_skip(pkg, snaps).is_none())
        .filter(|pkg| {
            matches!(
                resolve_view(
                    brew,
                    git,
                    snaps,
                    cache,
                    &pkg.origin,
                    &pkg.name,
                    pkg.kind,
                    Some(&pkg.receipt_rb)
                ),
                Ok(Some(view)) if bare_action(&view, greedy) == DesiredAction::InstallCutoff
            )
        })
        .count()
}

/// What a bare `upgrade`/`outdated` does: an installed self-updating cask
/// is left to the app, as brew leaves it without `--greedy` /
/// `--greedy-auto-updates`, whether it is behind the cutoff
/// (`InstallCutoff`), newer than it (`LeaveNewerThanCutoff`) or unordered
/// against it (`HoldUnordered`: the app owns its version, so there is
/// nothing to hold). `LeaveAheadOfSoak` and every other action are kept.
/// `greedy.latest` changes no action: a `:latest` cask is reinstalled only
/// when its cutoff definition differs from the installed one, which
/// `desired_action` already decides; see `latest_left` for the note.
pub(super) fn bare_action(view: &ResolvedView, greedy: flags::Greedy) -> DesiredAction {
    if view.auto_updates
        && !greedy.auto_updates
        && view.installed.is_some()
        && matches!(
            view.action,
            DesiredAction::InstallCutoff
                | DesiredAction::LeaveNewerThanCutoff
                | DesiredAction::HoldUnordered
        )
    {
        DesiredAction::LeaveAutoUpdates
    } else {
        view.action
    }
}

/// Under `--greedy` / `--greedy-latest`, brew reinstalls every `:latest`
/// cask. brewsoak leaves one whose cutoff definition matches the installed
/// one and says why. Evaluate after `bare_action` has settled the action.
pub(super) fn latest_left(view: &ResolvedView, greedy: flags::Greedy) -> bool {
    greedy.latest
        && view.latest
        && view.installed.is_some()
        && view.action == DesiredAction::NoOpAlreadySoaked
}

pub(super) fn latest_note(name: &str) -> String {
    format!(
        "{name}: version :latest left unchanged; the contents of a :latest cask cannot be soaked"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::test_support::summary_has;
    use crate::identity::PkgIdentity;

    use crate::brew::MockBrew;

    use crate::eligibility::DesiredAction;
    use crate::git::InMemoryGit;

    use crate::cmd::test_support::{
        AUTO_UPDATES_HEADER, OUTDATED_HEADER, auto_updates_world, cask_rb, cfg24, formula_pkg,
        formula_pkg_from, formula_rb, greedy, greedy_outdated, greedy_upgrade, greedy_world,
        inv_from, lock_runs, openssl_is_newer, run_is_soaked_install, section_lines, tap_snaps,
        tapped, unused_cache,
    };
    use crate::cmd::upgrade::upgrade;
    use crate::cmd::view::ResolvedView;

    #[test]
    fn bare_upgrade_total_leaves_auto_updates_casks_out() {
        let (brew, git, snaps) = auto_updates_world();
        let cfg = cfg24();
        let inv = inv_from(&brew, &cfg);
        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            tempfile::tempdir().unwrap().path(),
            tempfile::tempdir().unwrap().path(),
            &inv,
            &cfg,
            &[],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("Upgrading soaked packages (0 of 1)"),
            "{text}"
        );
    }

    /// A hand-built view for the pure functions. `latest` makes the cutoff
    /// `version :latest`; the installed side is always a numbered version.
    fn greedy_view(action: DesiredAction, auto_updates: bool, latest: bool) -> ResolvedView {
        let id = |v: &str| {
            PkgIdentity::Cask(
                crate::identity::parse_cask(&format!("cask \"x\" do\n  version {v}\nend\n"), None)
                    .unwrap(),
            )
        };
        ResolvedView {
            installed: Some(id("\"1.0\"")),
            cutoff: Some(id(if latest { ":latest" } else { "\"2.0\"" })),
            head: None,
            action,
            cutoff_blob: None,
            warnings: Vec::new(),
            auto_updates,
            latest,
            soaking: false,
        }
    }

    #[test]
    fn bare_upgrade_of_greedy_world_is_unchanged() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["-v"]);
        assert!(
            text.contains("Upgrading soaked packages (2 of 5)"),
            "{text}"
        );
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "wget"), "{text}");
        assert!(
            run_is_soaked_install(&runs, "moved"),
            "a changed :latest definition reinstalls: {text}"
        );
        assert!(!run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(!run_is_soaked_install(&runs, "self-latest"), "{text}");
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(
            !text.contains("cannot be soaked"),
            "bare run says nothing about :latest: {text}"
        );
        assert!(summary_has(&text, "auto-updates", "2"), "{text}");
    }

    #[test]
    fn upgrade_announces_a_moved_latest_cask_as_a_changed_definition() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy", "-v"]);
        assert!(
            text.contains("reinstalling moved (latest definition changed)"),
            "{text}"
        );
        assert!(!text.contains("upgrading moved"), "{text}");
        assert!(
            text.contains("moved: installed :latest definition differs from cutoff; reinstalling"),
            "{text}"
        );
    }

    #[test]
    fn greedy_upgrades_self_updating_casks_and_notes_latest_casks() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy", "-v"]);
        assert!(
            text.contains("Upgrading soaked packages (4 of 5)"),
            "{text}"
        );
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(run_is_soaked_install(&runs, "self-latest"), "{text}");
        assert!(run_is_soaked_install(&runs, "moved"), "{text}");
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(
            text.contains("alt-tab: installing cutoff 11.8.0; installed 7.38.1 is behind soak; installing cutoff"),
            "{text}"
        );
        assert!(
            text.contains("nightly: up to date (soaked); installed :latest matches cutoff; left unchanged; its contents cannot be soaked"),
            "{text}"
        );
        assert!(text.contains("\nNotes\n"), "{text}");
        assert!(
            text.contains(&format!("\n  {}\n", latest_note("nightly"))),
            "the note is a line of the notes block: {text}"
        );
        assert!(
            summary_has(&text, "auto-updates", "0"),
            "no cask was left to the app: {text}"
        );
        assert!(summary_has(&text, "upgraded", "4"), "{text}");
        assert!(summary_has(&text, "already soaked", "1"), "{text}");
        for args in &runs {
            assert!(
                !args.iter().any(|a| a.starts_with("--greedy")),
                "brew install never sees the greedy flags: {args:?}"
            );
        }
    }

    #[test]
    fn greedy_auto_updates_upgrades_self_updating_casks_only() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy-auto-updates"]);
        assert!(
            text.contains("Upgrading soaked packages (4 of 5)"),
            "{text}"
        );
        let runs = lock_runs(&brew);
        assert!(run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(run_is_soaked_install(&runs, "self-latest"), "{text}");
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(!text.contains("cannot be soaked"), "{text}");
    }

    #[test]
    fn greedy_latest_leaves_self_updating_casks_and_notes_latest() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy-latest", "-v"]);
        assert!(
            text.contains("Upgrading soaked packages (2 of 5)"),
            "{text}"
        );
        let runs = lock_runs(&brew);
        assert!(!run_is_soaked_install(&runs, "alt-tab"), "{text}");
        assert!(
            !run_is_soaked_install(&runs, "self-latest"),
            "both :latest and self-updating: the auto-updates leave stands: {text}"
        );
        assert!(run_is_soaked_install(&runs, "moved"), "{text}");
        assert!(
            text.contains("self-latest: auto-updates; installed :latest is left to the app"),
            "{text}"
        );
        assert!(text.contains(&latest_note("nightly")), "{text}");
        assert!(summary_has(&text, "auto-updates", "2"), "{text}");
    }

    #[test]
    fn short_g_is_greedy_on_upgrade_and_never_reaches_brew_install() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["-vg"]);
        assert!(
            text.contains("alt-tab: installing cutoff"),
            "-vg is brewsoak-verbose: {text}"
        );
        assert!(
            text.contains("Upgrading soaked packages (4 of 5)"),
            "{text}"
        );
        let installs: Vec<Vec<String>> = lock_runs(&brew)
            .into_iter()
            .filter(|a| a.first().map(String::as_str) == Some("install"))
            .collect();
        assert_eq!(installs.len(), 4, "{text}");
        for args in &installs {
            assert!(
                !args.iter().any(|a| a == "-g" || a == "-vg" || a == "--git"),
                "`g` is --git to brew install: {args:?}"
            );
            assert!(
                args.iter().any(|a| a == "-v"),
                "the rest of the cluster stays: {args:?}"
            );
        }
    }

    #[test]
    fn named_upgrade_ignores_greedy_and_prints_no_latest_note() {
        let (brew, git, snaps) = greedy_world();
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
            &["nightly".into(), "alt-tab".into()],
            &["--greedy".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let runs = lock_runs(&brew);
        assert!(!run_is_soaked_install(&runs, "nightly"), "{text}");
        assert!(
            run_is_soaked_install(&runs, "alt-tab"),
            "named: upgraded regardless of greedy: {text}"
        );
        assert!(!text.contains("cannot be soaked"), "{text}");
        assert!(!text.contains("does not accept --greedy"), "{text}");
    }

    #[test]
    fn bare_greedy_latest_upgrade_prints_the_latest_note_in_notes() {
        let (brew, git, snaps) = greedy_world();
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy-latest"]);
        let notes = text
            .split("\nNotes\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no Notes section: {text}"));
        assert!(notes.contains("nightly"), "{text}");
        assert!(notes.contains("cannot be soaked"), "{text}");
    }

    /// Installed `alt-tab` 7.38.1 (self-updating app) while the cutoff and
    /// HEAD cask say 7.0.0: the app updated itself past the cutoff.
    fn auto_updates_ahead_world() -> (MockBrew, InMemoryGit, Snapshots) {
        let (brew, git, snaps) = auto_updates_world();
        let rb = format!("{}  auto_updates true\n", cask_rb("alt-tab", "7.0.0"));
        git.insert_blob("caskcut", "Casks/a/alt-tab.rb", rb.clone());
        git.insert_blob("caskhead", "Casks/a/alt-tab.rb", rb);
        (brew, git, snaps)
    }

    const ALT_TAB_NOTE: &str = "alt-tab: installed 7.38.1 is newer than cutoff 7.0.0";

    #[test]
    fn bare_run_leaves_a_self_updating_cask_newer_than_its_cutoff_to_the_app() {
        let (mut brew, git, snaps) = auto_updates_ahead_world();
        brew.orderings.insert(
            ("7.38.1".to_string(), "7.0.0".to_string()),
            std::cmp::Ordering::Greater,
        );
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "alt-tab"),
            "{text}"
        );
        assert!(
            text.contains("Upgrading soaked packages (0 of 1)"),
            "{text}"
        );
        assert!(summary_has(&text, "ahead", "0"), "{text}");
        assert!(summary_has(&text, "auto-updates", "1"), "{text}");
        assert!(!text.contains(ALT_TAB_NOTE), "{text}");
        let text = greedy_outdated(&brew, &git, &snaps, &[]);
        assert!(section_lines(&text, OUTDATED_HEADER).is_empty(), "{text}");
        assert!(
            section_lines(&text, "==> Ahead of soak").is_empty(),
            "{text}"
        );
        assert_eq!(
            section_lines(&text, AUTO_UPDATES_HEADER),
            ["alt-tab (7.38.1) > 7.0.0"],
            "{text}"
        );
        for argv in [&["--greedy"][..], &["--greedy-auto-updates"][..]] {
            let text = greedy_upgrade(&brew, &git, &snaps, argv);
            assert!(summary_has(&text, "ahead", "1"), "{argv:?}: {text}");
            assert!(summary_has(&text, "auto-updates", "0"), "{argv:?}: {text}");
            assert!(text.contains(ALT_TAB_NOTE), "{argv:?}: {text}");
            let text = greedy_outdated(&brew, &git, &snaps, argv);
            assert_eq!(
                section_lines(&text, "==> Ahead of soak"),
                ["alt-tab (7.38.1) > 7.0.0"],
                "{argv:?}: {text}"
            );
            assert!(
                section_lines(&text, AUTO_UPDATES_HEADER).is_empty(),
                "{text}"
            );
        }
    }

    #[test]
    fn bare_run_leaves_a_self_updating_cask_brew_cannot_order_to_the_app() {
        let (mut brew, git, snaps) = auto_updates_ahead_world();
        brew.compare_error = Some("brew ruby exploded".into());
        let text = greedy_upgrade(&brew, &git, &snaps, &[]);
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "alt-tab"),
            "{text}"
        );
        assert!(
            summary_has(&text, "held", "0"),
            "not held on a bare run: {text}"
        );
        assert!(summary_has(&text, "auto-updates", "1"), "{text}");
        let text = greedy_outdated(&brew, &git, &snaps, &[]);
        assert!(section_lines(&text, "==> Held").is_empty(), "{text}");
        assert_eq!(
            section_lines(&text, AUTO_UPDATES_HEADER),
            ["alt-tab (7.38.1) ? 7.0.0"],
            "{text}"
        );
        let text = greedy_upgrade(&brew, &git, &snaps, &["--greedy"]);
        assert!(summary_has(&text, "held", "1"), "{text}");
    }

    /// Pins the claim in `plan_size`'s comment: the pre-run total, the
    /// `[i/N]` announcements and the `outdated` report agree in every mode.
    #[test]
    fn plan_size_upgrade_and_outdated_agree_under_every_greedy_mode() {
        for (argv, want) in [
            (&[][..], 2usize),
            (&["--greedy-latest"][..], 2),
            (&["--greedy-auto-updates"][..], 4),
            (&["--greedy"][..], 4),
        ] {
            let (mut brew, git, snaps) = greedy_world();
            // A re-pointed formula is in no mode's plan.
            brew.installed.push(formula_pkg(
                "openssl@3",
                formula_rb("openssl@3", "3.6.5", "sha365"),
            ));
            let rb = formula_rb("openssl@3", "3.5.9", "sha359");
            git.insert_blob("cutoffsha", "Formula/o/openssl@3.rb", rb.clone());
            git.insert_blob("headsha", "Formula/o/openssl@3.rb", rb);
            openssl_is_newer(&mut brew);
            let cfg = cfg24();
            let inv = inv_from(&brew, &cfg);
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let size = plan_size(
                &brew,
                &git,
                &snaps,
                unused_cache(),
                &inv,
                flags::greedy_mode(&owned),
            );
            assert_eq!(size, want, "plan_size {argv:?}");

            let text = greedy_upgrade(&brew, &git, &snaps, argv);
            assert!(
                text.contains(&format!("Upgrading soaked packages ({want} of 6)")),
                "{argv:?}: {text}"
            );
            let announced = text
                .lines()
                .filter(|l| l.trim_start().starts_with('[') && l.contains(&format!("/{want}] ")))
                .count();
            assert_eq!(announced, want, "announced {argv:?}: {text}");

            let text = greedy_outdated(&brew, &git, &snaps, argv);
            assert_eq!(
                section_lines(&text, OUTDATED_HEADER).len(),
                want,
                "outdated {argv:?}: {text}"
            );
        }
    }

    #[test]
    fn bare_action_leaves_self_updating_cask_unless_greedy_auto_updates() {
        let view = greedy_view(DesiredAction::InstallCutoff, true, false);
        assert_eq!(
            bare_action(&view, greedy(&[])),
            DesiredAction::LeaveAutoUpdates
        );
        assert_eq!(
            bare_action(&view, greedy(&["--greedy-latest"])),
            DesiredAction::LeaveAutoUpdates
        );
        assert_eq!(
            bare_action(&view, greedy(&["--greedy-auto-updates"])),
            DesiredAction::InstallCutoff
        );
        assert_eq!(
            bare_action(&view, greedy(&["--greedy"])),
            DesiredAction::InstallCutoff
        );
    }

    #[test]
    fn bare_action_leaves_a_self_updating_cask_newer_than_or_unordered_against_its_cutoff() {
        for action in [
            DesiredAction::LeaveNewerThanCutoff,
            DesiredAction::HoldUnordered,
        ] {
            let view = greedy_view(action, true, false);
            for argv in [&[][..], &["--greedy-latest"][..]] {
                assert_eq!(
                    bare_action(&view, greedy(argv)),
                    DesiredAction::LeaveAutoUpdates,
                    "{action:?} {argv:?}"
                );
            }
            for argv in [&["--greedy-auto-updates"][..], &["--greedy"][..]] {
                assert_eq!(
                    bare_action(&view, greedy(argv)),
                    action,
                    "{action:?} {argv:?}"
                );
            }
            let mut fresh = greedy_view(action, true, false);
            fresh.installed = None;
            assert_eq!(bare_action(&fresh, greedy(&[])), action, "{action:?}");
            let plain = greedy_view(action, false, false);
            assert_eq!(bare_action(&plain, greedy(&[])), action, "{action:?}");
        }
    }

    #[test]
    fn bare_action_only_changes_a_self_updating_install() {
        for argv in [
            &[][..],
            &["--greedy"][..],
            &["--greedy-latest"][..],
            &["--greedy-auto-updates"][..],
        ] {
            let plain = greedy_view(DesiredAction::InstallCutoff, false, false);
            assert_eq!(
                bare_action(&plain, greedy(argv)),
                DesiredAction::InstallCutoff,
                "{argv:?}"
            );
            let soaked = greedy_view(DesiredAction::NoOpAlreadySoaked, true, false);
            assert_eq!(
                bare_action(&soaked, greedy(argv)),
                DesiredAction::NoOpAlreadySoaked,
                "{argv:?}"
            );
            let ahead = greedy_view(DesiredAction::LeaveAheadOfSoak, true, false);
            assert_eq!(
                bare_action(&ahead, greedy(argv)),
                DesiredAction::LeaveAheadOfSoak,
                "{argv:?}"
            );
            let mut fresh = greedy_view(DesiredAction::InstallCutoff, true, false);
            fresh.installed = None;
            assert_eq!(
                bare_action(&fresh, greedy(argv)),
                DesiredAction::InstallCutoff,
                "{argv:?}"
            );
            for action in [
                DesiredAction::RefuseTooNew,
                DesiredAction::RefuseYanked,
                DesiredAction::RefuseDeprecated,
            ] {
                let view = greedy_view(action, true, false);
                assert_eq!(bare_action(&view, greedy(argv)), action, "{argv:?}");
            }
        }
    }

    #[test]
    fn latest_left_only_under_greedy_latest_with_a_matching_cutoff() {
        let same = greedy_view(DesiredAction::NoOpAlreadySoaked, false, true);
        assert!(
            !latest_left(&same, greedy(&[])),
            "bare run: silent, as today"
        );
        assert!(latest_left(&same, greedy(&["--greedy-latest"])));
        assert!(latest_left(&same, greedy(&["--greedy"])));
        assert!(!latest_left(&same, greedy(&["--greedy-auto-updates"])));
        let moved = greedy_view(DesiredAction::InstallCutoff, false, true);
        assert!(
            !latest_left(&moved, greedy(&["--greedy"])),
            "a changed definition reinstalls"
        );
        let numbered = greedy_view(DesiredAction::NoOpAlreadySoaked, false, false);
        assert!(!latest_left(&numbered, greedy(&["--greedy"])));
        let mut fresh = greedy_view(DesiredAction::NoOpAlreadySoaked, false, true);
        fresh.installed = None;
        assert!(!latest_left(&fresh, greedy(&["--greedy"])));
        assert_eq!(
            latest_note("nightly"),
            "nightly: version :latest left unchanged; the contents of a :latest cask cannot be soaked"
        );
    }

    #[test]
    fn auto_updates_cask_is_left_alone_on_bare_upgrade_but_upgraded_when_named() {
        let (brew, git, snaps) = auto_updates_world();
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
            &["-v".into()],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !run_is_soaked_install(&lock_runs(&brew), "alt-tab"),
            "bare run must not reinstall a self-updating app: {text}"
        );
        assert!(
            text.contains("alt-tab: auto-updates; installed 7.38.1 is left to the app"),
            "{text}"
        );
        for (label, value) in [
            ("upgraded", "0"),
            ("already soaked", "0"),
            ("held", "0"),
            ("ahead", "0"),
            ("pinned", "0"),
            ("skipped", "0"),
            ("no-soak", "0"),
            ("auto-updates", "1"),
        ] {
            assert!(summary_has(&text, label, value), "{label}: {text}");
        }

        let mut out = Vec::new();
        upgrade(
            &brew,
            &git,
            &snaps,
            cache.path(),
            tap.path(),
            &inv,
            &cfg,
            &["alt-tab".into()],
            &[],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("upgrading alt-tab 7.38.1 -> 11.8.0"),
            "{text}"
        );
        assert!(
            run_is_soaked_install(&lock_runs(&brew), "alt-tab"),
            "named explicitly, it upgrades like `brew upgrade alt-tab`: {text}"
        );
    }

    #[test]
    fn snapshotted_pkgs_skips_a_held_tap_that_keeps_its_last_good_snapshot() {
        let git = InMemoryGit::new();
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
        let inv = inv_from(&brew, &cfg24());
        let mut snaps = tap_snaps(&git, "hashicorp/tap", Some("tapcut"), "taphead");
        assert_eq!(snapshotted_pkgs(&inv, &snaps).count(), 1);
        snaps.held_taps.insert(
            "hashicorp/tap".into(),
            "while fetching history from origin, git failed:\nfatal: boom".into(),
        );
        assert!(snaps.tap("hashicorp/tap").is_some(), "stale table is kept");
        assert_eq!(snapshotted_pkgs(&inv, &snaps).count(), 0);
    }
}
