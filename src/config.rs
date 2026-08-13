use crate::{Error, SoakHours};
use std::path::Path;

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
        && let Some(hours) = parse_file(contents)
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

fn parse_file(contents: &str) -> Option<SoakHours> {
    let v: toml::Value = toml::from_str(contents).ok()?;
    let n = v.get("SOAK_HOURS")?.as_integer()?;
    let n = u32::try_from(n).ok()?;
    SoakHours::new(n)
}

pub fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn apply_persist(action: PersistAction, path: &Path) -> Result<(), Error> {
    match action {
        PersistAction::None => Ok(()),
        PersistAction::Write(hours) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, format!("SOAK_HOURS = {}\n", hours.get()))?;
            Ok(())
        }
        PersistAction::Delete => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        },
    }
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
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SOAK_HOURS = 48\n");
        apply_persist(PersistAction::Delete, &path).unwrap();
        assert!(!path.exists());
        apply_persist(PersistAction::Delete, &path).unwrap(); // missing is ok
    }
}
