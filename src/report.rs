use crate::SoakHours;
use crate::eligibility::DesiredAction;
use crate::identity::PkgIdentity;
use crate::inventory::PkgClass;
use crate::snapshot::TapSnapshot;
use time::OffsetDateTime;

pub fn format_utc(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute()
    )
}

pub fn short_sha(sha: &str) -> &str {
    if sha.len() > 8 { &sha[..8] } else { sha }
}

pub fn format_cutoff(sha: &str, time: Option<OffsetDateTime>) -> String {
    match time {
        Some(t) => format!("{} ({})", short_sha(sha), format_utc(t)),
        None => short_sha(sha).to_string(),
    }
}

pub fn soak_banner(doing: &str, hours: u32, core: &TapSnapshot, cask: &TapSnapshot) -> String {
    format!(
        "{doing}; soak window {hours}h; core cutoff {}; cask cutoff {}",
        format_cutoff(&core.cutoff_sha, core.cutoff_time),
        format_cutoff(&cask.cutoff_sha, cask.cutoff_time),
    )
}

/// The version as brew names it, in messages and in the Cellar path: `1.0.0`,
/// or `1.0.0_2` when the formula carries a revision. Showing the revision
/// keeps a revision-only upgrade from reading `1.0.0 -> 1.0.0`.
pub fn identity_version(id: &PkgIdentity) -> String {
    match id {
        PkgIdentity::Formula(f) if f.revision > 0 => format!("{}_{}", f.version, f.revision),
        PkgIdentity::Formula(f) => f.version.clone(),
        PkgIdentity::Cask(c) => c.version.clone(),
    }
}

/// What is soaking above an installed cutoff: HEAD's version, or a new build
/// of the same version when only the artifact changed (a rebuilt bottle, a
/// re-tagged source), so the note never reads `1.0.0 -> 1.0.0`.
pub fn soaking_label(cutoff: &PkgIdentity, head: &PkgIdentity) -> String {
    let (cut, hd) = (identity_version(cutoff), identity_version(head));
    if latest_redefined(Some(&cut), Some(&hd)) {
        "a new :latest definition".to_string()
    } else if cut == hd {
        format!("a new build of {hd}")
    } else {
        hd
    }
}

/// A named package left at its cutoff while a newer artifact soaks.
pub fn soaking_note(name: &str, installed: &str, label: &str, hours: SoakHours) -> String {
    format!(
        "{name}: {label} is still in the {}h soak window; installed {installed} is the newest soaked version",
        hours.get()
    )
}

/// A `:latest` cask has no version to order, so installed and cutoff both
/// reading `:latest` while the action is to install means the cask definition
/// (url or sha) changed, not that the installed one is "behind".
pub fn latest_redefined(installed: Option<&str>, cutoff: Option<&str>) -> bool {
    installed == Some(":latest") && cutoff == Some(":latest")
}

pub fn human_action(action: DesiredAction) -> &'static str {
    match action {
        DesiredAction::InstallCutoff => "would upgrade",
        DesiredAction::NoOpAlreadySoaked => "up to date (soaked)",
        DesiredAction::LeaveAheadOfSoak => "ahead of soak (leave installed)",
        DesiredAction::LeaveAutoUpdates => "auto-updates (left to the app; name it to upgrade)",
        DesiredAction::LeaveNewerThanCutoff => "ahead of soak (newer than cutoff; leave installed)",
        DesiredAction::HoldUnordered => "held: could not compare versions",
        DesiredAction::RefuseTooNew => "held: too new",
        DesiredAction::RefuseYanked => "held: yanked",
        DesiredAction::RefuseDeprecated => "held: deprecated",
    }
}

pub fn compact_info_line(
    name: &str,
    installed: Option<&PkgIdentity>,
    cutoff: Option<&PkgIdentity>,
    action: DesiredAction,
) -> String {
    let inst = installed
        .map(identity_version)
        .unwrap_or_else(|| "-".into());
    match action {
        DesiredAction::InstallCutoff => {
            let cut = cutoff.map(identity_version).unwrap_or_else(|| "?".into());
            if latest_redefined(Some(&inst), Some(&cut)) {
                return format!("{name}  {inst}  definition changed; would reinstall");
            }
            format!("{name}  {inst}  would upgrade to {cut}")
        }
        _ => format!("{name}  {inst}  {}", human_action(action)),
    }
}

pub fn evaluate_line(
    name: &str,
    action: DesiredAction,
    installed: Option<&PkgIdentity>,
    cutoff: Option<&PkgIdentity>,
    head: Option<&PkgIdentity>,
    soaking: bool,
    did: &str,
) -> String {
    let soaking_head = match (soaking, cutoff, head) {
        (true, Some(c), Some(h)) => Some(soaking_label(c, h)),
        _ => None,
    };
    let inst = installed.map(identity_version);
    let cut = cutoff.map(identity_version);
    let hd = head.map(identity_version);
    let (inst, cut, hd) = (inst.as_deref(), cut.as_deref(), hd.as_deref());
    let why = match action {
        DesiredAction::NoOpAlreadySoaked => match soaking_head {
            Some(label) => format!(
                "up to date (soaked); installed {} matches cutoff; HEAD {label} still soaking; {did}",
                inst.unwrap_or("?")
            ),
            None => format!(
                "up to date (soaked); installed {} matches cutoff; {did}",
                inst.unwrap_or("?")
            ),
        },
        DesiredAction::InstallCutoff if latest_redefined(inst, cut) => {
            format!("installed :latest definition differs from cutoff; reinstalling; {did}")
        }
        DesiredAction::InstallCutoff => {
            let state = match inst {
                Some(v) => format!("installed {v} is behind soak"),
                None => "not installed".to_string(),
            };
            format!("installing cutoff {}; {state}; {did}", cut.unwrap_or("?"))
        }
        DesiredAction::LeaveAheadOfSoak => format!(
            "ahead of soak; installed {} matches HEAD {}; {did}",
            inst.unwrap_or("?"),
            hd.unwrap_or("?")
        ),
        DesiredAction::LeaveNewerThanCutoff => format!(
            "ahead of soak; installed {} is newer than cutoff {}; {did}",
            inst.unwrap_or("?"),
            cut.unwrap_or("?")
        ),
        DesiredAction::HoldUnordered => format!(
            "held; could not compare installed {} with cutoff {}; {did}",
            inst.unwrap_or("?"),
            cut.unwrap_or("?")
        ),
        DesiredAction::LeaveAutoUpdates => format!(
            "auto-updates; installed {} is left to the app, cutoff {}; {did}",
            inst.unwrap_or("?"),
            cut.unwrap_or("?")
        ),
        DesiredAction::RefuseTooNew => {
            format!("held; too new (born inside the soak window); {did}")
        }
        DesiredAction::RefuseYanked => {
            format!("held; missing at HEAD (yanked); {did}")
        }
        DesiredAction::RefuseDeprecated => {
            format!("held; deprecated or disabled at HEAD; {did}")
        }
    };
    format!("{name}: {why}")
}

pub fn counts_line(c: &Counts) -> String {
    let verb = if c.dry_run {
        "would upgrade"
    } else {
        "upgraded"
    };
    let soaking = if c.soaking > 0 {
        format!(", soaking {}", c.soaking)
    } else {
        String::new()
    };
    let mut line = format!(
        "{verb} {}, already soaked {}{soaking}, held {}, ahead {}, pinned {}, skipped {}, no-soak {}",
        c.upgraded, c.soaked, c.held, c.ahead, c.pinned, c.skipped, c.no_soak
    );
    if c.auto_updates > 0 {
        line.push_str(&format!(", auto-updates {}", c.auto_updates));
    }
    line
}

pub fn origin_line(name: &str, origin: &str, hours: SoakHours, class: PkgClass) -> String {
    match class {
        PkgClass::Soaked => format!("{name}: origin {origin}; soak {}h", hours.get()),
        PkgClass::NoSoak => format!("{name}: origin {origin}; no-soak"),
        PkgClass::Unsoakable => format!("{name}: origin {origin}; unsoakable"),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    pub upgraded: usize,
    pub soaked: usize,
    /// At the cutoff with a newer artifact still soaking; not in `soaked`.
    pub soaking: usize,
    pub held: usize,
    pub ahead: usize,
    pub pinned: usize,
    pub skipped: usize,
    pub no_soak: usize,
    /// Self-updating casks a bare run left to the app (brew's `--greedy` skip).
    pub auto_updates: usize,
    /// A dry run: nothing was upgraded, so `upgraded` reads "would upgrade".
    pub dry_run: bool,
}

impl Counts {
    /// `soaking` is `eligibility::newer_soaking` for this package.
    pub fn note(&mut self, action: DesiredAction, soaking: bool) {
        match action {
            DesiredAction::InstallCutoff => self.upgraded += 1,
            DesiredAction::NoOpAlreadySoaked if soaking => self.soaking += 1,
            DesiredAction::NoOpAlreadySoaked => self.soaked += 1,
            DesiredAction::LeaveAheadOfSoak | DesiredAction::LeaveNewerThanCutoff => {
                self.ahead += 1
            }
            DesiredAction::LeaveAutoUpdates => self.auto_updates += 1,
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated
            | DesiredAction::HoldUnordered => self.held += 1,
        }
    }

    pub fn nothing_to_do(&self) -> bool {
        self.upgraded == 0 && self.held == 0 && self.no_soak == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_line_ends_with_no_soak() {
        let c = Counts {
            upgraded: 1,
            no_soak: 2,
            ..Counts::default()
        };
        assert_eq!(
            counts_line(&c),
            "upgraded 1, already soaked 0, held 0, ahead 0, pinned 0, skipped 0, no-soak 2"
        );
    }

    #[test]
    fn counts_line_shows_soaking_only_when_some_are() {
        let mut c = Counts::default();
        c.note(DesiredAction::NoOpAlreadySoaked, false);
        c.note(DesiredAction::NoOpAlreadySoaked, true);
        assert_eq!(
            counts_line(&c),
            "upgraded 0, already soaked 1, soaking 1, held 0, ahead 0, pinned 0, skipped 0, no-soak 0"
        );
    }

    #[test]
    fn soaking_label_names_a_new_build_of_the_same_version() {
        let cut = formula_id("1.0.0", 0);
        assert_eq!(soaking_label(&cut, &formula_id("1.1.0", 0)), "1.1.0");
        assert_eq!(
            soaking_label(&cut, &formula_id("1.0.0", 0)),
            "a new build of 1.0.0"
        );
        let latest = |url: &str| {
            PkgIdentity::Cask(crate::identity::CaskIdentity {
                version: ":latest".into(),
                sha256: Some(":no_check".into()),
                url: Some(url.into()),
            })
        };
        assert_eq!(
            soaking_label(&latest("https://a/1.dmg"), &latest("https://a/2.dmg")),
            "a new :latest definition"
        );
    }

    #[test]
    fn evaluate_line_names_the_soaking_head() {
        let cut = formula_id("14.0.0", 0);
        let head = formula_id("15.0.0", 0);
        let line = evaluate_line(
            "cats",
            DesiredAction::NoOpAlreadySoaked,
            Some(&cut),
            Some(&cut),
            Some(&head),
            true,
            "left unchanged",
        );
        assert_eq!(
            line,
            "cats: up to date (soaked); installed 14.0.0 matches cutoff; HEAD 15.0.0 still soaking; left unchanged"
        );
    }

    #[test]
    fn origin_line_shows_hours_or_class() {
        use crate::inventory::PkgClass;
        let h = crate::SoakHours::new(72).unwrap();
        assert_eq!(
            origin_line("terraform", "hashicorp/tap", h, PkgClass::Soaked),
            "terraform: origin hashicorp/tap; soak 72h"
        );
        assert_eq!(
            origin_line("brewsoak", "ericfitz/tap", h, PkgClass::NoSoak),
            "brewsoak: origin ericfitz/tap; no-soak"
        );
        assert_eq!(
            origin_line("x", "local/tap", h, PkgClass::Unsoakable),
            "x: origin local/tap; unsoakable"
        );
    }

    #[test]
    fn compact_line_for_upgrade() {
        let inst = PkgIdentity::Formula(crate::identity::FormulaIdentity {
            version: "1.0.0".into(),
            revision: 0,
            rebuild: None,
            sha256: "aaa".into(),
            derived_from: None,
        });
        let cut = PkgIdentity::Formula(crate::identity::FormulaIdentity {
            version: "1.1.0".into(),
            revision: 0,
            rebuild: None,
            sha256: "bbb".into(),
            derived_from: None,
        });
        let line = compact_info_line(
            "wget",
            Some(&inst),
            Some(&cut),
            DesiredAction::InstallCutoff,
        );
        assert_eq!(line, "wget  1.0.0  would upgrade to 1.1.0");
    }

    #[test]
    fn evaluate_line_says_not_installed_instead_of_installed_not_installed() {
        let cut = PkgIdentity::Formula(crate::identity::FormulaIdentity {
            version: "1.16.1".into(),
            revision: 0,
            rebuild: None,
            sha256: "bbb".into(),
            derived_from: None,
        });
        let line = evaluate_line(
            "packer",
            DesiredAction::InstallCutoff,
            None,
            Some(&cut),
            None,
            false,
            "installing cutoff",
        );
        assert_eq!(
            line,
            "packer: installing cutoff 1.16.1; not installed; installing cutoff"
        );
    }

    fn formula_id(version: &str, revision: u32) -> PkgIdentity {
        PkgIdentity::Formula(crate::identity::FormulaIdentity {
            version: version.into(),
            revision,
            rebuild: None,
            sha256: "s".into(),
            derived_from: None,
        })
    }

    #[test]
    fn evaluate_line_shows_the_revision_when_only_the_revision_differs() {
        let inst = formula_id("26.10.0", 1);
        let cut = formula_id("26.10.0", 2);
        let line = evaluate_line(
            "node",
            DesiredAction::InstallCutoff,
            Some(&inst),
            Some(&cut),
            None,
            false,
            "installing cutoff",
        );
        assert_eq!(
            line,
            "node: installing cutoff 26.10.0_2; installed 26.10.0_1 is behind soak; installing cutoff"
        );
    }

    #[test]
    fn compact_line_says_a_moved_latest_definition_changed() {
        let latest = || {
            PkgIdentity::Cask(crate::identity::CaskIdentity {
                version: ":latest".into(),
                sha256: Some(":no_check".into()),
                url: Some("https://example.com/new.dmg".into()),
            })
        };
        let line = compact_info_line(
            "moved",
            Some(&latest()),
            Some(&latest()),
            DesiredAction::InstallCutoff,
        );
        assert_eq!(line, "moved  :latest  definition changed; would reinstall");
    }

    #[test]
    fn compact_line_shows_the_revision() {
        let line = compact_info_line(
            "node",
            Some(&formula_id("26.10.0", 1)),
            Some(&formula_id("26.10.0", 2)),
            DesiredAction::InstallCutoff,
        );
        assert_eq!(line, "node  26.10.0_1  would upgrade to 26.10.0_2");
    }

    #[test]
    fn banner_includes_hours_and_cutoff() {
        let core = TapSnapshot {
            cutoff_sha: "abcdef012345".into(),
            head_sha: "ffff".into(),
            cutoff_time: None,
        };
        let line = soak_banner("upgrading", 24, &core, &core);
        assert!(line.contains("soak window 24h"), "{line}");
        assert!(line.contains("abcdef01"), "{line}");
    }
}
