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

pub fn human_action(action: DesiredAction) -> &'static str {
    match action {
        DesiredAction::InstallCutoff => "would upgrade",
        DesiredAction::NoOpAlreadySoaked => "up to date (soaked)",
        DesiredAction::LeaveAheadOfSoak => "ahead of soak (leave installed)",
        DesiredAction::LeaveAutoUpdates => "auto-updates (left to the app; name it to upgrade)",
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
    did: &str,
) -> String {
    let inst = installed.map(identity_version);
    let cut = cutoff.map(identity_version);
    let hd = head.map(identity_version);
    let (inst, cut, hd) = (inst.as_deref(), cut.as_deref(), hd.as_deref());
    let why = match action {
        DesiredAction::NoOpAlreadySoaked => format!(
            "up to date (soaked); installed {} matches cutoff; {did}",
            inst.unwrap_or("?")
        ),
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
    let mut line = format!(
        "{verb} {}, already soaked {}, held {}, ahead {}, pinned {}, skipped {}, no-soak {}",
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
    pub fn note(&mut self, action: DesiredAction) {
        match action {
            DesiredAction::InstallCutoff => self.upgraded += 1,
            DesiredAction::NoOpAlreadySoaked => self.soaked += 1,
            DesiredAction::LeaveAheadOfSoak => self.ahead += 1,
            DesiredAction::LeaveAutoUpdates => self.auto_updates += 1,
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => self.held += 1,
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
        });
        let cut = PkgIdentity::Formula(crate::identity::FormulaIdentity {
            version: "1.1.0".into(),
            revision: 0,
            rebuild: None,
            sha256: "bbb".into(),
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
        });
        let line = evaluate_line(
            "packer",
            DesiredAction::InstallCutoff,
            None,
            Some(&cut),
            None,
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
            "installing cutoff",
        );
        assert_eq!(
            line,
            "node: installing cutoff 26.10.0_2; installed 26.10.0_1 is behind soak; installing cutoff"
        );
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
