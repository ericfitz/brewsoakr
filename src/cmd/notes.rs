use crate::Error;
use crate::eligibility::DesiredAction;
use std::io::Write;

pub fn refusal_message(action: DesiredAction, name: &str, brew_verb: &str) -> Option<String> {
    let why = match action {
        DesiredAction::RefuseTooNew => format!("{name} is too new (born inside the soak window)"),
        DesiredAction::RefuseYanked => format!("{name} is missing at HEAD (yanked)"),
        DesiredAction::RefuseDeprecated => format!("{name} is deprecated or disabled at HEAD"),
        DesiredAction::HoldUnordered => {
            format!("{name}: brew could not compare its installed version with the cutoff")
        }
        DesiredAction::NoOpAlreadySoaked
        | DesiredAction::LeaveAheadOfSoak
        | DesiredAction::LeaveNewerThanCutoff
        | DesiredAction::LeaveAutoUpdates
        | DesiredAction::InstallCutoff => return None,
    };
    Some(format!(
        "{why}; use `brew {brew_verb} {name}` to bypass brewsoak."
    ))
}

pub fn ahead_message(name: &str) -> String {
    format!("{name} is ahead of soak; leaving installed artifact unchanged")
}

pub fn newer_than_cutoff_note(name: &str, installed: &str, cutoff: &str) -> String {
    format!(
        "{name}: installed {installed} is newer than cutoff {cutoff}; leaving it (upstream may have re-pointed it)"
    )
}

/// Why a package cannot be soaked, worded for a note.
pub(super) fn unsoakable_note(name: &str, origin_tap: &str, tapped: bool) -> String {
    if tapped {
        format!(
            "{name}: tap {origin_tap} has no HTTPS remote; not soakable. Use brew, or add it to NO_SOAK"
        )
    } else {
        format!(
            "{name}: tap {origin_tap} is not installed; run brew tap {origin_tap} first, or add it to NO_SOAK"
        )
    }
}

/// Detail text (often git's multi-line stderr) on its own lines, each
/// indented two spaces so it reads as belonging to the line above it.
pub(super) fn indented_detail(detail: &str) -> String {
    detail
        .lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn held_tap_note(name: &str, origin_tap: &str, err: &str) -> String {
    format!(
        "{name}: tap {origin_tap} could not be refreshed; held.\n{}",
        indented_detail(err)
    )
}

pub(super) fn hold_why(action: DesiredAction) -> Option<&'static str> {
    match action {
        DesiredAction::RefuseTooNew => Some("too new (born inside the soak window)"),
        DesiredAction::RefuseYanked => Some("missing at HEAD (yanked)"),
        DesiredAction::RefuseDeprecated => Some("deprecated or disabled at HEAD"),
        DesiredAction::HoldUnordered => {
            Some("could not compare the installed version with the cutoff (brew)")
        }
        DesiredAction::NoOpAlreadySoaked
        | DesiredAction::LeaveAheadOfSoak
        | DesiredAction::LeaveNewerThanCutoff
        | DesiredAction::LeaveAutoUpdates
        | DesiredAction::InstallCutoff => None,
    }
}

pub(super) fn write_section(
    out: &mut impl Write,
    header: &str,
    lines: &[String],
) -> Result<(), Error> {
    if lines.is_empty() {
        return Ok(());
    }
    write_section_always(out, header, lines)
}

/// Caveats brew printed and where the raw brew log went.
pub(super) fn write_session_tail(
    report: &crate::brew::SessionReport,
    out: &mut impl Write,
) -> Result<(), Error> {
    for line in &report.caveats {
        writeln!(out, "{line}")?;
    }
    if let Some(path) = &report.log_path {
        writeln!(out, "full brew log: {}", path.display())?;
    }
    Ok(())
}

pub(super) fn write_section_always(
    out: &mut impl Write,
    header: &str,
    lines: &[String],
) -> Result<(), Error> {
    writeln!(out, "{header}")?;
    if lines.is_empty() {
        writeln!(out, "(none)")?;
    } else {
        for line in lines {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::eligibility::DesiredAction;

    fn assert_refuse_copy(action: DesiredAction, name: &str, brew_verb: &str) {
        let msg = refusal_message(action, name, brew_verb).expect("refuse message");
        assert!(
            msg.contains(&format!("brew {brew_verb} {name}")),
            "{action:?} missing `brew {brew_verb} {name}`: {msg}"
        );
        assert!(
            !msg.contains("--now"),
            "{action:?} must not mention --now: {msg}"
        );
    }

    #[test]
    fn refuse_too_new_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseTooNew, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseTooNew, "wget", "upgrade");
    }

    #[test]
    fn refuse_yanked_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseYanked, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseYanked, "wget", "upgrade");
    }

    #[test]
    fn refuse_deprecated_mentions_brew_verb_not_now() {
        assert_refuse_copy(DesiredAction::RefuseDeprecated, "wget", "install");
        assert_refuse_copy(DesiredAction::RefuseDeprecated, "wget", "upgrade");
    }

    #[test]
    fn leave_ahead_of_soak_is_not_a_refusal() {
        assert_eq!(
            refusal_message(DesiredAction::LeaveAheadOfSoak, "wget", "upgrade"),
            None
        );
    }
}
