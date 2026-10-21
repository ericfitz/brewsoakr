use crate::Error;
use crate::eligibility::DesiredAction;
use crate::section::Section;
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

/// Where a deferred note sorts in the `Notes` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoteKind {
    /// A package brewsoak refused or could not act on.
    Held,
    /// Everything else, listed with no sub-heading.
    Other,
}

/// A note on its own lines: continuation lines sit two spaces deeper.
fn write_note(out: &mut impl Write, indent: &str, note: &str) -> Result<(), Error> {
    for (i, line) in note.lines().enumerate() {
        if i == 0 {
            writeln!(out, "{indent}{line}")?;
        } else {
            writeln!(out, "{indent}  {}", line.trim_start())?;
        }
    }
    Ok(())
}

/// The `Notes` section: one entry per line, grouped by kind. Empty groups,
/// and the whole section when nothing is in it, are omitted.
pub(super) fn write_notes(
    out: &mut impl Write,
    soaking: &[String],
    ahead: &[String],
    notes: &[(NoteKind, String)],
) -> Result<(), Error> {
    let mut sec = Section::new(out, "Notes");
    if !soaking.is_empty() {
        writeln!(sec, "Still soaking (left at the cutoff):")?;
        for entry in soaking {
            writeln!(sec, "  {entry}")?;
        }
    }
    if !ahead.is_empty() {
        writeln!(sec, "Ahead of soak (installed artifact left unchanged):")?;
        for name in ahead {
            writeln!(sec, "  {name}")?;
        }
    }
    if notes.iter().any(|(k, _)| *k == NoteKind::Held) {
        writeln!(sec, "Held:")?;
        for (_, note) in notes.iter().filter(|(k, _)| *k == NoteKind::Held) {
            write_note(&mut sec, "  ", note)?;
        }
    }
    for (_, note) in notes.iter().filter(|(k, _)| *k == NoteKind::Other) {
        write_note(&mut sec, "", note)?;
    }
    Ok(())
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
    let mut caveats = Section::new(&mut *out, "Caveats");
    for line in &report.caveats {
        writeln!(caveats, "{line}")?;
    }
    drop(caveats);
    if let Some(path) = &report.log_path {
        writeln!(out, "Full brew log: {}", path.display())?;
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
    fn notes_grouped_one_per_line() {
        let soaking = ["awscli 2.38.3".to_string(), "c-ares 1.34.9".to_string()];
        let ahead = ["uv".to_string()];
        let notes = [
            (NoteKind::Other, "plain note".to_string()),
            (
                NoteKind::Held,
                "foo: too new; use `brew upgrade foo`.".to_string(),
            ),
            (
                NoteKind::Other,
                "bar: tap x/y could not be refreshed.\n  git failed\n  fatal".to_string(),
            ),
            (NoteKind::Held, "baz: held".to_string()),
        ];
        let mut out = Vec::new();
        write_notes(&mut out, &soaking, &ahead, &notes).unwrap();
        let want = "\
Notes
  Still soaking (left at the cutoff):
    awscli 2.38.3
    c-ares 1.34.9
  Ahead of soak (installed artifact left unchanged):
    uv
  Held:
    foo: too new; use `brew upgrade foo`.
    baz: held
  plain note
  bar: tap x/y could not be refreshed.
    git failed
    fatal
";
        assert_eq!(String::from_utf8(out).unwrap(), want);
        let mut empty = Vec::new();
        write_notes(&mut empty, &[], &[], &[]).unwrap();
        assert!(empty.is_empty());
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
