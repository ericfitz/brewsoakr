use crate::eligibility::DesiredAction;
use crate::identity::PkgIdentity;
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

pub fn identity_version(id: &PkgIdentity) -> &str {
    match id {
        PkgIdentity::Formula(f) => f.version.as_str(),
        PkgIdentity::Cask(c) => c.version.as_str(),
    }
}

pub fn human_action(action: DesiredAction) -> &'static str {
    match action {
        DesiredAction::InstallCutoff => "would upgrade",
        DesiredAction::NoOpAlreadySoaked => "up to date (soaked)",
        DesiredAction::LeaveAheadOfSoak => "ahead of soak (leave installed)",
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
    let inst = installed.map(identity_version).unwrap_or("-");
    match action {
        DesiredAction::InstallCutoff => {
            let cut = cutoff.map(identity_version).unwrap_or("?");
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
    let why = match action {
        DesiredAction::NoOpAlreadySoaked => format!(
            "up to date (soaked); installed {} matches cutoff; {did}",
            inst.unwrap_or("?")
        ),
        DesiredAction::InstallCutoff => format!(
            "installing cutoff {}; installed {} is behind soak; {did}",
            cut.unwrap_or("?"),
            inst.unwrap_or("not installed")
        ),
        DesiredAction::LeaveAheadOfSoak => format!(
            "ahead of soak; installed {} matches HEAD {}; {did}",
            inst.unwrap_or("?"),
            hd.unwrap_or("?")
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
    format!(
        "upgraded {}, already soaked {}, held {}, ahead {}, pinned {}, skipped {}",
        c.upgraded, c.soaked, c.held, c.ahead, c.pinned, c.skipped
    )
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    pub upgraded: usize,
    pub soaked: usize,
    pub held: usize,
    pub ahead: usize,
    pub pinned: usize,
    pub skipped: usize,
}

impl Counts {
    pub fn note(&mut self, action: DesiredAction) {
        match action {
            DesiredAction::InstallCutoff => self.upgraded += 1,
            DesiredAction::NoOpAlreadySoaked => self.soaked += 1,
            DesiredAction::LeaveAheadOfSoak => self.ahead += 1,
            DesiredAction::RefuseTooNew
            | DesiredAction::RefuseYanked
            | DesiredAction::RefuseDeprecated => self.held += 1,
        }
    }

    pub fn nothing_to_do(&self) -> bool {
        self.upgraded == 0 && self.held == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
