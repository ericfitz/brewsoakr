use crate::nosoak::{self, NoSoakList};
use crate::{Error, SoakHours};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapEntry {
    /// `user/repo`, lowercased.
    pub name: String,
    pub soak_hours: Option<SoakHours>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedFile {
    pub soak_hours: Option<SoakHours>,
    pub taps: Vec<TapEntry>,
    pub no_soak: NoSoakList,
    /// Invalid entries, worded for `-v`.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Resolved `SOAK_HOURS`: CLI > env > file > 24.
    pub hours: SoakHours,
    pub persist: PersistAction,
    pub taps: Vec<TapEntry>,
    pub no_soak: NoSoakList,
    pub notes: Vec<String>,
}

impl Config {
    pub fn uniform(hours: SoakHours) -> Self {
        Self {
            hours,
            persist: PersistAction::None,
            taps: Vec::new(),
            no_soak: NoSoakList::default(),
            notes: Vec::new(),
        }
    }

    /// `NO_SOAK` is checked by the caller first; this is steps 2 and 3 of the
    /// spec's "effective soak hours".
    pub fn effective_hours(&self, origin: &str) -> SoakHours {
        let origin = origin.to_ascii_lowercase();
        self.taps
            .iter()
            .find(|t| t.name == origin)
            .and_then(|t| t.soak_hours)
            .unwrap_or(self.hours)
    }

    pub fn is_no_soak(&self, origin: &str, name: &str) -> bool {
        self.no_soak.matches(origin, name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistAction {
    None,
    Write(SoakHours),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedHours {
    pub hours: SoakHours,
    pub persist: PersistAction,
}

pub fn resolve_hours(
    cli: Option<u32>,
    env: Option<&str>,
    file_contents: Option<&str>,
) -> Result<ResolvedHours, Error> {
    if let Some(n) = cli {
        let hours = SoakHours::new(n)
            .ok_or_else(|| Error::Usage("--soak-hours must be an integer >= 1".into()))?;
        let persist = if hours == SoakHours::DEFAULT {
            PersistAction::Delete
        } else {
            PersistAction::Write(hours)
        };
        return Ok(ResolvedHours { hours, persist });
    }
    if let Some(raw) = env
        && let Some(hours) = raw.parse::<u32>().ok().and_then(SoakHours::new)
    {
        return Ok(ResolvedHours {
            hours,
            persist: PersistAction::None,
        });
    }
    if let Some(contents) = file_contents
        && let Some(hours) = parse_file(contents).soak_hours
    {
        return Ok(ResolvedHours {
            hours,
            persist: PersistAction::None,
        });
    }
    Ok(ResolvedHours {
        hours: SoakHours::DEFAULT,
        persist: PersistAction::None,
    })
}

pub fn resolve_config(
    cli: Option<u32>,
    env: Option<&str>,
    file_contents: Option<&str>,
) -> Result<Config, Error> {
    let resolved = resolve_hours(cli, env, file_contents)?;
    let parsed = file_contents.map(parse_file).unwrap_or_default();
    Ok(Config {
        hours: resolved.hours,
        persist: resolved.persist,
        taps: parsed.taps,
        no_soak: parsed.no_soak,
        notes: parsed.notes,
    })
}

/// Bad TOML is silently all-defaults (v1 rule). Bad keys or entries fall back
/// individually and leave a note.
pub fn parse_file(contents: &str) -> ParsedFile {
    let Ok(v) = toml::from_str::<toml::Value>(contents) else {
        return ParsedFile::default();
    };
    let mut out = ParsedFile {
        soak_hours: v
            .get("SOAK_HOURS")
            .and_then(toml::Value::as_integer)
            .and_then(|n| u32::try_from(n).ok())
            .and_then(SoakHours::new),
        ..ParsedFile::default()
    };
    parse_taps(&v, &mut out);
    parse_no_soak(&v, &mut out);
    out
}

fn parse_taps(v: &toml::Value, out: &mut ParsedFile) {
    let Some(entries) = v.get("TAP") else {
        return;
    };
    let Some(entries) = entries.as_array() else {
        out.notes
            .push("config: TAP is not an array of tables; ignored".into());
        return;
    };
    for entry in entries {
        let Some(name) = entry.get("name").and_then(toml::Value::as_str) else {
            out.notes
                .push("config: [[TAP]] entry is missing name; skipped".into());
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower.split('/').count() != 2 || lower.split('/').any(str::is_empty) {
            out.notes.push(format!(
                "config: [[TAP]] name {name:?} is not user/repo; skipped"
            ));
            continue;
        }
        let soak_hours = match entry.get("soak_hours") {
            None => None,
            Some(h) => match h
                .as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .and_then(SoakHours::new)
            {
                Some(h) => Some(h),
                None => {
                    out.notes.push(format!(
                        "config: [[TAP]] {lower} soak_hours {h} is not an integer >= 1; using SOAK_HOURS"
                    ));
                    None
                }
            },
        };
        if let Some(pos) = out.taps.iter().position(|t| t.name == lower) {
            out.notes.push(format!(
                "config: duplicate [[TAP]] {lower}; last entry wins"
            ));
            out.taps.remove(pos);
        }
        out.taps.push(TapEntry {
            name: lower,
            soak_hours,
        });
    }
}

fn parse_no_soak(v: &toml::Value, out: &mut ParsedFile) {
    let Some(raw) = v.get("NO_SOAK") else {
        return;
    };
    let strings: Option<Vec<&str>> = raw
        .as_array()
        .and_then(|a| a.iter().map(toml::Value::as_str).collect());
    let Some(strings) = strings else {
        out.notes
            .push("config: NO_SOAK is not an array of strings; ignored".into());
        return;
    };
    let mut entries = Vec::new();
    for s in strings {
        match nosoak::parse_entry(s) {
            Ok(e) => entries.push(e),
            Err(reason) => out.notes.push(format!("config: {reason}")),
        }
    }
    out.no_soak = NoSoakList::new(entries);
}

pub fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Key-level edit of `SOAK_HOURS`. Returns a warning instead of touching a
/// file that is not valid TOML. Comments are not preserved.
pub fn apply_persist(action: PersistAction, path: &Path) -> Result<Option<String>, Error> {
    if action == PersistAction::None {
        return Ok(None);
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut table = match existing.as_deref() {
        None => toml::Table::new(),
        Some(s) => match toml::from_str::<toml::Table>(s) {
            Ok(t) => t,
            Err(_) => {
                return Ok(Some(format!(
                    "{} is not valid TOML; --soak-hours was not persisted (it still applies to this run)",
                    path.display()
                )));
            }
        },
    };
    match action {
        PersistAction::Write(hours) => {
            table.insert(
                "SOAK_HOURS".into(),
                toml::Value::Integer(i64::from(hours.get())),
            );
        }
        PersistAction::Delete => {
            table.remove("SOAK_HOURS");
        }
        PersistAction::None => unreachable!(),
    }
    if table.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = toml::to_string(&table).map_err(|e| Error::Other(format!("config: {e}")))?;
    std::fs::write(path, body)?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_when_nothing_set() {
        let r = resolve_hours(None, None, None).unwrap();
        assert_eq!(r.hours.get(), 24);
        assert_eq!(r.persist, PersistAction::None);
    }

    #[test]
    fn cli_wins_and_persists() {
        let r = resolve_hours(Some(48), Some("12"), Some("SOAK_HOURS = 6\n")).unwrap();
        assert_eq!(r.hours.get(), 48);
        assert_eq!(r.persist, PersistAction::Write(SoakHours::new(48).unwrap()));
    }

    #[test]
    fn cli_24_deletes() {
        let r = resolve_hours(Some(24), Some("48"), None).unwrap();
        assert_eq!(r.hours.get(), 24);
        assert_eq!(r.persist, PersistAction::Delete);
    }

    #[test]
    fn cli_zero_is_usage() {
        assert!(matches!(
            resolve_hours(Some(0), None, None),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn env_used_when_no_cli() {
        let r = resolve_hours(None, Some("36"), Some("SOAK_HOURS = 6\n")).unwrap();
        assert_eq!(r.hours.get(), 36);
        assert_eq!(r.persist, PersistAction::None);
    }

    #[test]
    fn invalid_env_falls_through() {
        let r = resolve_hours(None, Some("nope"), Some("SOAK_HOURS = 8\n")).unwrap();
        assert_eq!(r.hours.get(), 8);
    }

    #[test]
    fn invalid_file_is_default() {
        for contents in [
            "",
            "hours = 48\n",
            "SOAK_HOURS = 0\n",
            "SOAK_HOURS = \"x\"\n",
            "[[[",
        ] {
            let r = resolve_hours(None, None, Some(contents)).unwrap();
            assert_eq!(r.hours.get(), 24, "contents={contents:?}");
        }
    }

    #[test]
    fn apply_write_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        apply_persist(PersistAction::Write(SoakHours::new(48).unwrap()), &path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "SOAK_HOURS = 48"
        );
        apply_persist(PersistAction::Delete, &path).unwrap();
        assert!(!path.exists());
        apply_persist(PersistAction::Delete, &path).unwrap(); // missing is ok
    }

    const FULL: &str = r#"
SOAK_HOURS = 48
NO_SOAK = ["ericfitz/tap", "wget", "hashicorp/tap/terraform"]

[[TAP]]
name = "HashiCorp/tap"
soak_hours = 72

[[TAP]]
name = "cyclonedx/cyclonedx"
"#;

    #[test]
    fn parse_file_reads_taps_and_no_soak() {
        let p = parse_file(FULL);
        assert_eq!(p.soak_hours.map(|h| h.get()), Some(48));
        assert_eq!(p.taps.len(), 2);
        assert_eq!(p.taps[0].name, "hashicorp/tap");
        assert_eq!(p.taps[0].soak_hours.map(|h| h.get()), Some(72));
        assert_eq!(p.taps[1].name, "cyclonedx/cyclonedx");
        assert_eq!(p.taps[1].soak_hours, None);
        assert!(p.no_soak.matches("ericfitz/tap", "brewsoak"));
        assert!(p.no_soak.matches("homebrew/core", "wget"));
        assert!(p.no_soak.matches("hashicorp/tap", "terraform"));
        assert!(!p.no_soak.matches("hashicorp/tap", "vault"));
        assert!(p.notes.is_empty(), "{:?}", p.notes);
    }

    #[test]
    fn effective_hours_prefers_tap_entry_then_global() {
        let cfg = resolve_config(None, None, Some(FULL)).unwrap();
        assert_eq!(cfg.effective_hours("hashicorp/tap").get(), 72);
        assert_eq!(cfg.effective_hours("HASHICORP/TAP").get(), 72);
        assert_eq!(cfg.effective_hours("cyclonedx/cyclonedx").get(), 48);
        assert_eq!(cfg.effective_hours("homebrew/core").get(), 48);
        assert!(cfg.is_no_soak("ericfitz/tap", "brewsoak"));
    }

    #[test]
    fn core_and_cask_can_have_their_own_hours() {
        let cfg = resolve_config(
            None,
            None,
            Some("[[TAP]]\nname = \"homebrew/core\"\nsoak_hours = 96\n"),
        )
        .unwrap();
        assert_eq!(cfg.effective_hours("homebrew/core").get(), 96);
        assert_eq!(cfg.effective_hours("homebrew/cask").get(), 24);
    }

    #[test]
    fn cli_hours_still_win_over_file_but_not_over_tap_entry() {
        let cfg = resolve_config(Some(12), None, Some(FULL)).unwrap();
        assert_eq!(cfg.hours.get(), 12);
        assert_eq!(cfg.effective_hours("homebrew/core").get(), 12);
        assert_eq!(cfg.effective_hours("hashicorp/tap").get(), 72);
    }

    #[test]
    fn invalid_tap_entries_fall_back_and_are_noted() {
        let p = parse_file(
            "[[TAP]]\nname = \"a/b\"\nsoak_hours = 0\n\n[[TAP]]\nsoak_hours = 5\n\n[[TAP]]\nname = \"bad\"\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 7\n",
        );
        assert_eq!(p.taps.len(), 1, "{:?}", p.taps);
        assert_eq!(p.taps[0].name, "a/b");
        assert_eq!(
            p.taps[0].soak_hours.map(|h| h.get()),
            Some(7),
            "last duplicate wins"
        );
        assert_eq!(p.notes.len(), 4, "{:?}", p.notes);
        assert!(
            p.notes
                .iter()
                .any(|n| n.contains("soak_hours") && n.contains("a/b")),
            "{:?}",
            p.notes
        );
        assert!(
            p.notes
                .iter()
                .any(|n| n.contains("missing") && n.contains("name")),
            "{:?}",
            p.notes
        );
        assert!(
            p.notes.iter().any(|n| n.contains("\"bad\"")),
            "{:?}",
            p.notes
        );
        assert!(
            p.notes.iter().any(|n| n.contains("duplicate")),
            "{:?}",
            p.notes
        );
    }

    #[test]
    fn no_soak_not_an_array_of_strings_is_ignored_and_noted() {
        for contents in [
            "NO_SOAK = \"wget\"\n",
            "NO_SOAK = [1, 2]\n",
            "NO_SOAK = [\"wget\", 3]\n",
        ] {
            let p = parse_file(contents);
            assert!(p.no_soak.is_empty(), "{contents:?}");
            assert!(
                p.notes.iter().any(|n| n.contains("NO_SOAK")),
                "{contents:?}: {:?}",
                p.notes
            );
        }
    }

    #[test]
    fn no_soak_malformed_entry_is_skipped_and_noted() {
        let p = parse_file("NO_SOAK = [\"wget\", \"a/b/c/d\", \"\"]\n");
        assert!(p.no_soak.matches("homebrew/core", "wget"));
        assert!(!p.no_soak.matches("a/b", "c"));
        assert_eq!(p.notes.len(), 2, "{:?}", p.notes);
    }

    #[test]
    fn bad_toml_is_all_defaults() {
        let p = parse_file("[[[");
        assert_eq!(p, ParsedFile::default());
    }

    #[test]
    fn persist_write_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        let warn = apply_persist(PersistAction::Write(SoakHours::new(12).unwrap()), &path).unwrap();
        assert_eq!(warn, None);
        let p = parse_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(p.soak_hours.map(|h| h.get()), Some(12));
        assert_eq!(p.taps.len(), 2);
        assert!(p.no_soak.matches("ericfitz/tap", "x"));
    }

    #[test]
    fn persist_delete_removes_only_soak_hours_and_file_only_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        apply_persist(PersistAction::Delete, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("SOAK_HOURS"), "{text}");
        assert!(text.contains("NO_SOAK"), "{text}");
        std::fs::write(&path, "SOAK_HOURS = 48\n").unwrap();
        apply_persist(PersistAction::Delete, &path).unwrap();
        assert!(!path.exists(), "file with no keys left must be deleted");
    }

    #[test]
    fn persist_leaves_invalid_toml_alone_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[[").unwrap();
        let warn = apply_persist(PersistAction::Write(SoakHours::new(12).unwrap()), &path).unwrap();
        assert!(warn.is_some_and(|w| w.contains("not valid TOML")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[[");
        let warn = apply_persist(PersistAction::Delete, &path).unwrap();
        assert!(warn.is_some());
        assert!(path.exists());
    }
}
